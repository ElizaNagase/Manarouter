//! The candidate list: which paths to try when no fetcher knows where a provider publishes a quota.
//!
//! Every other reader in this module is written for a provider somebody already reverse-engineered:
//! it knows the URL, the auth and the response shape, and hard-codes all three. That leaves the long
//! tail — a self-hosted gateway, a reseller, a provider nobody has written a fetcher for — with
//! nothing: [`crate::quota::openai_compat::OpenAiCompat`] can read a quota *if the operator declares
//! `quotaEndpoint`*, and otherwise reports that it knows nothing.
//!
//! This module is the middle ground. There is no standard place an OpenAI-compatible upstream
//! publishes a balance, but there are only a handful of places anybody uses, so the router tries
//! them **in order and stops at the first that answers**. The list is configuration
//! ([`QuotaProbeConfig`], the `quotaProbe` section of the config file), an account may override it,
//! and the walk never leaves the account's own base URL.
//!
//! # The rules the walk keeps
//!
//! * **Relative paths only.** A candidate is a path (`/v1/balance`), never a URL. Every request is
//!   assembled from the *origin* of the account's own `base_url` plus that path, and the assembled
//!   URL is parsed and its host compared with the origin's before anything is sent. A declaration
//!   carrying a scheme or a second host is a configuration error naming the entry, not a request to
//!   somebody else's server — a config file that can make the router fetch an arbitrary host is a
//!   way to leak a credential, and the credential is exactly what a probe sends.
//! * **404 and 405 mean "not here", everything else non-2xx means "stop".** Those two statuses are
//!   what an upstream returns for a path it does not serve, which is the expected answer for most of
//!   the candidates. A 500 or a 429 is a statement about the upstream, not about the path, and
//!   walking on would be hammering a provider that is already unhappy.
//! * **Bounded.** The default list is nine paths plus at most one `POST`, and a configured list is
//!   truncated at [`MAX_PROBE_REQUESTS`]. One unreachable host therefore costs one pass, not a loop.
//! * **A 2xx is only an answer if a number came back with it.** The body is read by the *existing*
//!   conventional extractor ([`crate::quota::openai_compat::conventional_quota`]) — including its
//!   refusal to emit a row with no `total` — so this module adds no second opinion about what a
//!   quota body looks like. A 2xx whose body resolves nothing is a candidate that did not answer,
//!   and the walk continues.
//! * **Headers are the fallback, and only the fallback.** Some upstreams report the remaining
//!   allowance in `x-ratelimit-*`, `x-balance-*` or `x-credit*` response headers and never in a
//!   body. Those are read **only** when no candidate's body produced a quota, and a limit with no
//!   consumption figure attached is reported with `fraction_reported = false` rather than as a
//!   `used == total` guess. A `0` limit is not a denominator: no `0/0` row is ever emitted.
//! * **Off is off.** Nothing here runs on a timer. The walk happens inside one
//!   [`QuotaFetcher::fetch`](crate::quota::registry::QuotaFetcher::fetch) call, which the
//!   control plane makes only when the quota subsystem is enabled, and `quotaProbe.enabled = false`
//!   stops it inside as well.
//!
//! # The one candidate that is not a `GET`
//!
//! The last candidate is `POST /v1/chat/completions` with a one-token body, read for its *headers*:
//! it is the only endpoint an OpenAI-compatible upstream is guaranteed to have, so when nothing else
//! is served it is the last place a rate-limit header can come from. Its body is never a request for
//! a completion anyone reads, and it **does** spend a token of real quota, which is why it runs last
//! and only when a model id has been configured for it (`quotaProbe.model`). No model id is
//! hard-coded here: inventing one would send a probe to a model the account may not have.

use std::collections::BTreeMap;

use http::Uri;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::quota::http::{QuotaRequest, QuotaResponse};
use crate::quota::openai_compat::{conventional_quota, OpenAiCompat, QUOTA_ENDPOINT_TIMEOUT};
use crate::quota::registry::{
    QuotaAccount, QuotaCredential, QuotaFetchContext, QUOTA_ENDPOINT_KEY,
};
use crate::quota::types::UsageQuota;

/// The account-extra key that overrides the candidate list, the sibling of
/// [`QUOTA_ENDPOINT_KEY`](crate::quota::registry::QUOTA_ENDPOINT_KEY).
///
/// A path list, or an object with `paths`, `enabled`, `chat_completion_probe` and `model`:
///
/// ```json
/// { "quotaProbe": ["/v1/credits", "/v1/balance"] }
/// ```
///
/// ```json
/// { "quotaProbe": { "paths": ["/api/v1/key"], "model": "the-model-this-account-uses" } }
/// ```
pub const QUOTA_PROBE_KEY: &str = "quotaProbe";

/// The paths tried, in order, by default.
///
/// Ordered by how commonly an OpenAI-compatible upstream serves them: the two `balance` spellings a
/// reseller usually exposes, the two `key` spellings a self-hosted gateway usually exposes, then the
/// per-user and billing shapes, then the plain usage ones. Every one of them is a `GET`.
pub const DEFAULT_QUOTA_PROBE_PATHS: [&str; 9] = [
    "/v1/balance",
    "/api/balance",
    "/api/v1/key",
    "/v1/key",
    "/v1/user/info",
    "/v1/dashboard/billing/subscription",
    "/v1/credits",
    "/v1/quota",
    "/v1/usage",
];

/// The one candidate that is a `POST`, tried after every path in the list.
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// Hard ceiling on how many requests one walk may make, however long the configured list is.
///
/// The default list needs ten. The ceiling exists because the list is operator-editable and an
/// account's credential is spent on every request: a list of two hundred paths under one account
/// would be a denial of service against the account itself.
pub const MAX_PROBE_REQUESTS: usize = 25;

/// Statuses that mean "this upstream does not serve that path", and so end one candidate rather
/// than the walk.
const UNSUPPORTED_STATUSES: [u16; 2] = [404, 405];

/// The header families read when no body answered, in the order they are preferred.
///
/// `None` for the limit means the family is never reported: without a denominator there is no
/// fraction, and every construction of one here would be an invention.
const HEADER_FAMILIES: [(&str, &[&str], &[&str]); 3] = [
    (
        "requests",
        &["x-ratelimit-limit-requests"],
        &["x-ratelimit-remaining-requests"],
    ),
    (
        "tokens",
        &["x-ratelimit-limit-tokens"],
        &["x-ratelimit-remaining-tokens"],
    ),
    (
        "credits",
        &[
            "x-credit-limit",
            "x-credit-total",
            "x-balance-limit",
            "x-balance-total",
            "x-balance",
        ],
        &[
            "x-credit-remaining",
            "x-credit-balance",
            "x-balance-remaining",
            "x-balance-available",
        ],
    ),
];

/// Which candidate paths to try, and whether to try them at all.
///
/// This is the type of the root configuration section — `quotaProbe` in the config file, next to
/// `quota`, with `quota_probe` accepted as the snake_case spelling to match the rest of the file —
/// and of one account's own declaration under the same `quotaProbe` key in its `extra`, which wins
/// over it. Both default to [`DEFAULT_QUOTA_PROBE_PATHS`], so a configuration written before this
/// existed keeps the documented behaviour without being rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaProbeConfig {
    /// Whether the walk runs. `false` means the fallback reader reports that it knows nothing,
    /// exactly as it did before this section existed.
    pub enabled: bool,
    /// The candidate paths, in the order they are tried.
    pub paths: Vec<String>,
    /// Whether `POST /v1/chat/completions` is tried after the paths. Turn it off for an account
    /// where spending a token on a probe is unacceptable.
    #[serde(alias = "chatCompletionProbe")]
    pub chat_completion_probe: bool,
    /// Anything else the operator wrote here, preserved across a round trip.
    ///
    /// `model` is read from it when the one-token `POST` candidate runs: the probe has to name a
    /// model, and naming one here — rather than in this file — keeps a model id an operator's
    /// choice and not a hard-coded guess.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for QuotaProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: DEFAULT_QUOTA_PROBE_PATHS
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            chat_completion_probe: true,
            extra: Map::new(),
        }
    }
}

/// Why one candidate path cannot be probed, or `None` when it is usable.
///
/// Shared by config validation — so a typo is reported when the file is loaded, naming the entry —
/// and by the walk itself, which refuses the same entries at runtime. The second check is not
/// redundant: an account's own `quotaProbe` list never passes through the config validator.
pub fn probe_path_problem(path: &str) -> Option<String> {
    if path.trim().is_empty() {
        return Some(format!("the quota probe entry {path:?} is empty"));
    }
    if path.contains("://") || path.starts_with("//") {
        return Some(format!(
            "the quota probe entry {path:?} is an absolute URL; a candidate is a path on the \
             account's own base URL, so it must start with a single `/` and name no host"
        ));
    }
    if !path.starts_with('/') {
        return Some(format!(
            "the quota probe entry {path:?} must start with `/`: a candidate is a path on the \
             provider's own base URL, not a URL"
        ));
    }
    if path.contains('#') {
        return Some(format!(
            "the quota probe entry {path:?} carries a fragment, which is never sent to a server"
        ));
    }
    if let Some(bad) = path
        .chars()
        .find(|character| character.is_control() || *character == ' ')
    {
        return Some(format!(
            "the quota probe entry {path:?} contains a space or control character ({bad:?})"
        ));
    }
    None
}

/// Check every entry of a candidate list, naming the one that is wrong.
///
/// `label` is what the message calls the list (`quotaProbe.paths`, or `quotaProbe` for an account's
/// inline declaration).
pub fn validate_probe_paths(label: &str, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            probe_path_problem(path).map(|problem| format!("{label}[{index}]: {problem}"))
        })
        .collect()
}

/// The list the walk uses, after validation.
fn usable_paths(label: &str, paths: &[String]) -> Result<Vec<String>> {
    match validate_probe_paths(label, paths).into_iter().next() {
        Some(problem) => Err(Error::Config(problem)),
        None => Ok(paths.to_vec()),
    }
}

/// The origin of a configured base URL, as `(origin, host)`.
///
/// `None` when the value names no host, or names one over a scheme this reader will not use. The
/// parse is deliberately as tolerant as [`crate::quota::registry::base_url_host`]'s — a base URL in
/// a config file is typed by a person, and `api.example.test/v1` is as good as
/// `https://api.example.test/v1` — but it keeps the **port**, which is the part that is easy to
/// drop and the part a self-hosted gateway needs.
fn probe_origin(base_url: &str) -> Option<(String, String)> {
    let trimmed = base_url.trim();
    let (scheme, rest) = match trimmed.find("://") {
        Some(index) => (&trimmed[..index], &trimmed[index + 3..]),
        None => ("https", trimmed),
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // The authority ends at the first `/`, `?` or `#`; the rest is the base path, which a candidate
    // path replaces rather than appends to.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // `user:password@host`: everything up to the last `@` is userinfo, and a credential in a base
    // URL is not something a probe should resend from a different one.
    let authority = match authority.rfind('@') {
        Some(index) => &authority[index + 1..],
        None => authority,
    };
    if authority.is_empty() {
        return None;
    }
    let origin = format!("{scheme}://{authority}");
    let uri: Uri = origin.parse().ok()?;
    let host = uri.host()?.to_owned();
    if uri.query().is_some() || uri.path() != "/" {
        // The origin is scheme and authority only; anything else means the value did not parse the
        // way it was read above, so it is refused rather than guessed at.
        return None;
    }
    Some((origin, host))
}

/// One candidate's URL: the account's own origin plus a relative path, or a configuration error.
///
/// The assembled URL is parsed again and its host compared with the origin's. With the path check
/// above, that comparison cannot fail today; it is here because it is the invariant that keeps a
/// credential on the host it belongs to, and an invariant that is only argued for in a comment is
/// one refactor away from being untrue.
fn probe_url(origin: &str, host: &str, path: &str) -> Result<String> {
    if let Some(problem) = probe_path_problem(path) {
        return Err(Error::Config(problem));
    }
    let url = format!("{origin}{path}");
    let uri: Uri = url.parse().map_err(|_| {
        Error::Config(format!(
            "the quota probe entry {path:?} is not a usable request path"
        ))
    })?;
    match uri.host() {
        Some(other) if other == host => Ok(url),
        _ => Err(Error::Config(format!(
            "the quota probe entry {path:?} does not resolve to the account's own host, so it was \
             not requested"
        ))),
    }
}

/// One request built from a candidate.
fn probe_request(url: String, credential: Option<&QuotaCredential>) -> QuotaRequest {
    let request = QuotaRequest::get(url)
        .header("accept", "application/json")
        .timeout(QUOTA_ENDPOINT_TIMEOUT);
    match credential {
        Some(credential) => request.bearer(credential.expose()),
        None => request,
    }
}

/// What one candidate's reply means for the walk.
enum Outcome {
    /// A quota was read; the walk is over and this is the answer.
    Answered(UsageQuota),
    /// This candidate did not answer. The `Some` is a header-only row held in reserve for the end of
    /// the walk.
    Continue(Option<UsageQuota>),
    /// Stop the walk here.
    Stop(Error),
}

/// Read one reply.
fn interpret(response: &QuotaResponse, provider_id: &str, path: &str) -> Outcome {
    if UNSUPPORTED_STATUSES.contains(&response.status) {
        tracing::debug!(provider = %provider_id, path, status = response.status, "candidate quota path is not served");
        return Outcome::Continue(None);
    }
    if !response.is_success() {
        return Outcome::Stop(Error::InvalidState(format!(
            "provider {provider_id:?} answered {} for the quota probe {path:?}",
            response.status
        )));
    }

    // The conventional extractor decides whether the body is a statement about a quota at all. Its
    // `NotFound` is the walk's "nothing resolvable here" — including its refusal of a body with no
    // `total`, which is what keeps a `0/0` row out of the panel.
    if let Ok(body) = response.json() {
        match conventional_quota(&body, provider_id) {
            Ok(quota) => {
                return Outcome::Answered(quota.with_source(probe_source(path, false)));
            }
            Err(Error::NotFound(reason)) => {
                tracing::debug!(provider = %provider_id, path, reason = %reason, "candidate quota path answered without a quota");
            }
            Err(other) => return Outcome::Stop(other),
        }
    }
    Outcome::Continue(
        quota_from_headers(response).map(|quota| quota.with_source(probe_source(path, true))),
    )
}

/// The `source` the walk stamps on a reading it produced: the fetcher, the candidate that answered,
/// and whether the number came from that candidate's body or from its response headers.
///
/// This is not a log line. `source` is the one field the cached snapshot carries about *where* a
/// reading came from, so the panel shows it back to the operator — `read from /v1/balance` — and the
/// shape below is the contract that read relies on. It keeps the fetcher id first, so a reader that
/// only knows the old value still sees a name it recognises.
fn probe_source(path: &str, from_headers: bool) -> String {
    if from_headers {
        format!("{} {path} (headers)", OpenAiCompat::ID)
    } else {
        format!("{} {path}", OpenAiCompat::ID)
    }
}

/// A numeric header value, if the header is present and reads as a finite number.
fn header_number(response: &QuotaResponse, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| response.header(name))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// The last resort: a quota read from response headers.
///
/// Only a family with a **positive** limit is reported. A limit with a remaining figure becomes a
/// reported fraction; a limit on its own becomes a row whose fraction was *not* reported, which is
/// the same rule [`crate::quota::openai_compat::map_quota`] applies to a `total` with no
/// consumption, and for the same reason: `used = total - 0` would forge an exhausted account.
fn quota_from_headers(response: &QuotaResponse) -> Option<UsageQuota> {
    for (label, limit_keys, remaining_keys) in HEADER_FAMILIES {
        let Some(limit) = header_number(response, limit_keys) else {
            continue;
        };
        if limit <= 0.0 {
            // Not a denominator. `x-ratelimit-limit-requests: 0` is a provider saying "no limit
            // applies", not a fraction of zero, so no row is emitted for it.
            continue;
        }
        let quota = match header_number(response, remaining_keys) {
            Some(remaining) => UsageQuota::from_counts(limit - remaining, limit, None),
            None => UsageQuota::from_counts_unreported(limit, limit, None),
        };
        return Some(
            quota
                .with_source(OpenAiCompat::ID)
                .with_display_name(format!("rate limit ({label})")),
        );
    }
    None
}

/// The one field of the declaration that is not a path: the model the one-token `POST` names.
fn probe_model(config: &QuotaProbeConfig) -> Result<Option<String>> {
    match config.extra.get("model") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(model)) => {
            let model = model.trim();
            Ok((!model.is_empty()).then(|| model.to_owned()))
        }
        Some(_) => Err(Error::Config(
            "the quota probe `model` must be a string (the model id the one-token \
             `POST /v1/chat/completions` probe may name)"
                .to_owned(),
        )),
    }
}

/// A list of paths, or a configuration error naming what was not a string.
fn string_list(label: &str, value: &Value) -> Result<Vec<String>> {
    let items = value
        .as_array()
        .ok_or_else(|| Error::Config(format!("the account's `{label}` must be a list of paths")))?;
    items
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                Error::Config(format!(
                    "the account's `{label}` entries must be strings; found {item}"
                ))
            })
        })
        .collect()
}

/// A boolean field, or a configuration error.
fn bool_field(label: &str, key: &str, value: &Value) -> Result<bool> {
    value.as_bool().ok_or_else(|| {
        Error::Config(format!(
            "the account's `{label}` `{key}` must be true or false; found {value}"
        ))
    })
}

/// The account's own declaration folded over the configured list.
///
/// A list wins for the paths and leaves everything else as configured; an object may set any of the
/// three fields and inherits the rest, so an account that only wants a different model does not
/// have to repeat the list.
fn account_probe(configured: &QuotaProbeConfig, declared: &Value) -> Result<QuotaProbeConfig> {
    match declared {
        Value::Array(_) => Ok(QuotaProbeConfig {
            paths: string_list(QUOTA_PROBE_KEY, declared)?,
            ..configured.clone()
        }),
        Value::Object(map) => {
            let mut config = configured.clone();
            for (key, value) in map {
                match key.as_str() {
                    "enabled" => config.enabled = bool_field(QUOTA_PROBE_KEY, key, value)?,
                    "paths" => {
                        config.paths = string_list(&format!("{QUOTA_PROBE_KEY}.paths"), value)?
                    }
                    "chat_completion_probe" | "chatCompletionProbe" => {
                        config.chat_completion_probe = bool_field(QUOTA_PROBE_KEY, key, value)?
                    }
                    _ => {
                        config.extra.insert(key.clone(), value.clone());
                    }
                }
            }
            Ok(config)
        }
        _ => Err(Error::Config(format!(
            "the account's `{QUOTA_PROBE_KEY}` must be a list of paths, or an object with `paths`, \
             `enabled`, `chat_completion_probe` and `model`"
        ))),
    }
}

/// The list to walk: the account's declaration, else the caller's configuration, else the default.
pub fn resolve_config(
    extra: &BTreeMap<String, Value>,
    configured: Option<&QuotaProbeConfig>,
) -> Result<QuotaProbeConfig> {
    let configured = configured.cloned().unwrap_or_default();
    match extra.get(QUOTA_PROBE_KEY) {
        None | Some(Value::Null) => Ok(configured),
        Some(declared) => account_probe(&configured, declared),
    }
}

/// The configured list for an account, with the account's own declaration folded in.
pub fn config_for(ctx: &QuotaFetchContext) -> Result<QuotaProbeConfig> {
    resolve_config(&ctx.account.extra, ctx.quota_probe.as_ref())
}

/// Walk the candidate list, returning the first quota anything reports.
///
/// `Err(Error::NotFound)` means every candidate was tried and none of them said anything about a
/// quota — the normal answer for a provider that has no such endpoint, and the reason this is a
/// fallback rather than a guess. `Err(Error::Config)` means the declaration is unusable and
/// **nothing was sent**. Any other error means a candidate stopped the walk.
pub async fn walk(ctx: &QuotaFetchContext) -> Result<UsageQuota> {
    let config = config_for(ctx)?;
    if !config.enabled {
        return Err(Error::NotFound(format!(
            "provider {:?} has no dedicated quota fetcher, declares no `{QUOTA_ENDPOINT_KEY}`, and \
             the `{QUOTA_PROBE_KEY}` walk is switched off",
            ctx.provider_id
        )));
    }
    let paths = usable_paths("quotaProbe.paths", &config.paths)?;
    let credential = ctx
        .credential
        .as_ref()
        .filter(|credential| !credential.is_empty());
    let Some(base_url) = ctx.base_url.as_deref() else {
        return Err(Error::NotFound(format!(
            "provider {:?} has no dedicated quota fetcher, declares no `{QUOTA_ENDPOINT_KEY}`, and \
             the account has no base URL to probe",
            ctx.provider_id
        )));
    };
    let Some((origin, host)) = probe_origin(base_url) else {
        return Err(Error::Config(format!(
            "provider {:?} has no usable base URL to probe (a candidate path is relative to its \
             host)",
            ctx.provider_id
        )));
    };

    let mut attempted = 0usize;
    // Held in reserve: only a walk that never read a quota from a body falls back to it.
    let mut from_headers: Option<UsageQuota> = None;
    tracing::debug!(
        provider = %ctx.provider_id,
        candidates = paths.len().min(MAX_PROBE_REQUESTS),
        "probing candidate quota paths"
    );

    for path in paths.iter().take(MAX_PROBE_REQUESTS) {
        let url = probe_url(&origin, &host, path)?;
        attempted += 1;
        let response = ctx.http.request(probe_request(url, credential)).await?;
        match interpret(&response, &ctx.provider_id, path) {
            Outcome::Answered(quota) => return Ok(quota),
            Outcome::Continue(row) => from_headers = from_headers.or(row),
            Outcome::Stop(error) => {
                tracing::debug!(provider = %ctx.provider_id, path, "quota probe stopped early");
                return match from_headers {
                    Some(quota) => Ok(quota),
                    None => Err(error),
                };
            }
        }
    }

    if config.chat_completion_probe && attempted < MAX_PROBE_REQUESTS {
        match probe_model(&config)? {
            Some(model) => {
                let url = probe_url(&origin, &host, CHAT_COMPLETIONS_PATH)?;
                attempted += 1;
                let body = json!({
                    "model": model,
                    "max_tokens": 1,
                    "messages": [{ "role": "user", "content": "hi" }],
                })
                .to_string();
                let request = QuotaRequest::post_json(url, body)
                    .header("accept", "application/json")
                    .timeout(QUOTA_ENDPOINT_TIMEOUT);
                let request = match credential {
                    Some(credential) => request.bearer(credential.expose()),
                    None => request,
                };
                let response = ctx.http.request(request).await?;
                match interpret(&response, &ctx.provider_id, CHAT_COMPLETIONS_PATH) {
                    Outcome::Answered(quota) => return Ok(quota),
                    Outcome::Continue(row) => from_headers = from_headers.or(row),
                    Outcome::Stop(error) => {
                        return match from_headers {
                            Some(quota) => Ok(quota),
                            None => Err(error),
                        };
                    }
                }
            }
            None => tracing::debug!(
                provider = %ctx.provider_id,
                "a configured `{QUOTA_PROBE_KEY}` `model` is needed to probe {}; skipping it",
                CHAT_COMPLETIONS_PATH
            ),
        }
    }

    match from_headers {
        Some(quota) => Ok(quota),
        None => Err(Error::NotFound(format!(
            "provider {:?} has no dedicated quota fetcher and none of the {} candidate quota paths \
             on its base URL reported a quota",
            ctx.provider_id, attempted
        ))),
    }
}

/// Whether an account asks for the walk at all — the cheap half of [`resolve_config`], for a
/// caller that wants to know before building a context.
pub fn declared_for(account: &QuotaAccount) -> bool {
    account.extra.contains_key(QUOTA_PROBE_KEY)
}

/// Whether a walk would actually be attempted for an account, given the configured section.
///
/// This is the question a caller asks *before* building a fetch context: an `openai-compatible`
/// provider has no dedicated fetcher and declares no `quotaEndpoint`, so a refresh is only worth
/// offering when something will really be sent. The list is resolved exactly as [`walk`] resolves
/// it, so an account that switches the walk off — or empties the list — is not offered and does not
/// collect an "it reported nothing" row on the quota page.
///
/// An **unreadable** declaration counts as `true`: that is a configuration mistake, and a refresh is
/// the one place that reports it in the operator's own terms rather than hiding it.
pub fn walk_configured(
    configured: Option<&QuotaProbeConfig>,
    extra: &BTreeMap<String, Value>,
) -> bool {
    match resolve_config(extra, configured) {
        Err(_) => true,
        Ok(config) => config.enabled && !config.paths.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::*;
    use crate::dialect::Dialect;
    use crate::quota::http::QuotaHttp;

    /// A transport double with one scripted reply per call, in order, remembering every request.
    ///
    /// Running out of scripted replies is an error rather than a repeat: a walk that makes more
    /// requests than a test scripted is a bug in the walk, and a repeated reply would hide it.
    #[derive(Debug)]
    struct ScriptedHttp {
        replies: Mutex<Vec<QuotaResponse>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl ScriptedHttp {
        fn new(replies: Vec<QuotaResponse>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("not poisoned").clone()
        }

        fn urls(&self) -> Vec<String> {
            self.requests().into_iter().map(|seen| seen.url).collect()
        }
    }

    #[async_trait]
    impl QuotaHttp for ScriptedHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen
                .lock()
                .expect("not poisoned")
                .push(request.clone());
            let mut replies = self.replies.lock().expect("not poisoned");
            if replies.is_empty() {
                return Err(Error::InvalidState(
                    "the scripted transport ran out of replies".to_owned(),
                ));
            }
            Ok(replies.remove(0))
        }
    }

    fn reply(status: u16, body: &str) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from(body.to_owned()),
        }
    }

    fn reply_with(status: u16, headers: &[(&str, &str)]) -> QuotaResponse {
        reply_body_with(status, "", headers)
    }

    /// A reply carrying both a body and headers, for the case where both matter.
    fn reply_body_with(status: u16, body: &str, headers: &[(&str, &str)]) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Bytes::from(body.to_owned()),
        }
    }

    /// The statuses a provider that serves no quota endpoint returns for the whole default list.
    fn unsupported() -> Vec<QuotaResponse> {
        (0..10).map(|_| reply(404, "not found")).collect()
    }

    fn context(fake: &Arc<ScriptedHttp>, account: QuotaAccount) -> QuotaFetchContext {
        QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(fake) as Arc<dyn QuotaHttp>,
        )
        .with_account(account)
        .with_base_url("https://api.example.test/v1")
        .with_credential("probe-secret")
    }

    fn account(declared: Value) -> QuotaAccount {
        QuotaAccount::new().with_extra(QUOTA_PROBE_KEY, declared)
    }

    async fn fetch(fake: &Arc<ScriptedHttp>, account: QuotaAccount) -> Result<UsageQuota> {
        walk(&context(fake, account)).await
    }

    const CONVENTIONAL: &str = r#"{"used":30,"total":100}"#;

    #[test]
    fn the_default_list_is_the_documented_one() {
        let config = QuotaProbeConfig::default();
        assert!(config.enabled);
        assert!(config.chat_completion_probe);
        assert_eq!(
            config.paths,
            vec![
                "/v1/balance",
                "/api/balance",
                "/api/v1/key",
                "/v1/key",
                "/v1/user/info",
                "/v1/dashboard/billing/subscription",
                "/v1/credits",
                "/v1/quota",
                "/v1/usage",
            ]
        );
        // A configuration file written before this section existed deserialises to the same thing.
        let empty: QuotaProbeConfig = serde_json::from_str("{}").expect("empty section");
        assert_eq!(empty, config);
        let round_trip: QuotaProbeConfig =
            serde_json::from_str(&serde_json::to_string(&config).expect("serialize"))
                .expect("round trip");
        assert_eq!(round_trip, config);
    }

    #[test]
    fn only_relative_paths_are_accepted() {
        assert!(probe_path_problem("/v1/quota").is_none());
        assert!(probe_path_problem("/v1/quota?account=7").is_none());

        let absolute = probe_path_problem("https://evil.test/v1/balance").expect("refused");
        assert!(
            absolute.contains("https://evil.test/v1/balance"),
            "{absolute}"
        );
        let protocol_relative = probe_path_problem("//evil.test/v1/balance").expect("refused");
        assert!(
            protocol_relative.contains("//evil.test/v1/balance"),
            "{protocol_relative}"
        );
        let no_slash = probe_path_problem("v1/balance").expect("refused");
        assert!(no_slash.contains("v1/balance"), "{no_slash}");
        assert!(probe_path_problem("").is_some());

        let problems = validate_probe_paths(
            "quotaProbe.paths",
            &["/v1/ok".to_owned(), "evil.test/x".to_owned()],
        );
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].starts_with("quotaProbe.paths[1]:"),
            "{problems:?}"
        );
        assert!(problems[0].contains("evil.test/x"), "{problems:?}");
    }

    #[test]
    fn a_base_url_keeps_its_host_and_port() {
        assert_eq!(
            probe_origin("https://api.example.test/v1"),
            Some((
                "https://api.example.test".to_owned(),
                "api.example.test".to_owned()
            ))
        );
        assert_eq!(
            probe_origin("http://127.0.0.1:8080/v1"),
            Some(("http://127.0.0.1:8080".to_owned(), "127.0.0.1".to_owned()))
        );
        assert_eq!(
            probe_origin("api.example.test/v1").map(|(_, host)| host),
            Some("api.example.test".to_owned())
        );
        assert_eq!(probe_origin("https://"), None);
        assert_eq!(
            probe_origin("file:///etc/passwd").map(|(_, host)| host),
            None
        );
    }

    #[tokio::test]
    async fn the_walk_tries_every_candidate_in_order_and_stops_at_the_first_answer() {
        let mut replies = unsupported();
        replies[2] = reply(200, CONVENTIONAL);
        let fake = ScriptedHttp::new(replies);

        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("third answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.used, 30.0);
        assert_eq!(
            fake.urls(),
            vec![
                "https://api.example.test/v1/balance",
                "https://api.example.test/api/balance",
                "https://api.example.test/api/v1/key",
            ]
        );
    }

    #[tokio::test]
    async fn the_reading_names_the_candidate_and_the_part_of_the_reply_it_came_from() {
        // A body answer names the path that carried it.
        let mut replies = vec![reply(404, "{}"), reply(200, CONVENTIONAL)];
        replies.extend(unsupported());
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(&fake, account(json!({ "paths": ["/alpha", "/beta"] })))
            .await
            .expect("the second candidate answers");
        assert_eq!(
            quota.source.as_deref(),
            Some(format!("{} /beta", OpenAiCompat::ID).as_str()),
            "the panel shows this back to the operator"
        );

        // A header-only answer says so, so a header row is never mistaken for a body that stated
        // the allowance in full.
        let mut replies = vec![reply_with(200, &[("x-ratelimit-limit-requests", "500")])];
        replies.extend(unsupported());
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(&fake, account(json!({ "paths": ["/alpha"] })))
            .await
            .expect("headers answer");
        assert_eq!(
            quota.source.as_deref(),
            Some(format!("{} /alpha (headers)", OpenAiCompat::ID).as_str())
        );
    }

    #[test]
    fn only_a_list_that_would_really_be_walked_is_offered() {
        // The default list, with the section at its default, is walked.
        assert!(walk_configured(None, &BTreeMap::new()));

        // Switching it off — at either level, or by emptying the list — offers nothing.
        let off = QuotaProbeConfig {
            enabled: false,
            ..QuotaProbeConfig::default()
        };
        assert!(!walk_configured(Some(&off), &BTreeMap::new()));
        let mut extra = BTreeMap::new();
        extra.insert(QUOTA_PROBE_KEY.to_owned(), json!({ "enabled": false }));
        assert!(!walk_configured(None, &extra));
        let mut extra = BTreeMap::new();
        extra.insert(QUOTA_PROBE_KEY.to_owned(), json!([]));
        assert!(
            !walk_configured(None, &extra),
            "an empty account list probes nothing"
        );
        let empty = QuotaProbeConfig {
            paths: Vec::new(),
            ..QuotaProbeConfig::default()
        };
        assert!(!walk_configured(Some(&empty), &BTreeMap::new()));

        // A declaration that cannot be read is offered anyway: the refresh is what reports it.
        let mut extra = BTreeMap::new();
        extra.insert(
            QUOTA_PROBE_KEY.to_owned(),
            json!("https://elsewhere.test/x"),
        );
        assert!(walk_configured(None, &extra));
    }

    #[tokio::test]
    async fn unsupported_statuses_continue_and_other_failures_stop() {
        // Both "not here" statuses keep the walk alive: 404 then 405 then an answer.
        let mut replies = vec![reply(404, "{}"), reply(405, "{}"), reply(200, CONVENTIONAL)];
        replies.extend(unsupported());
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(&fake, QuotaAccount::new()).await.expect("answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(fake.requests().len(), 3);

        // Anything else is a statement about the upstream: one request, then stop.
        for status in [500u16, 429, 403, 400] {
            let fake = ScriptedHttp::new(vec![reply(status, "{}"), reply(200, CONVENTIONAL)]);
            let error = fetch(&fake, QuotaAccount::new()).await.expect_err("stops");
            assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
            assert!(
                error.to_string().contains(&status.to_string()),
                "the status is named: {error}"
            );
            assert_eq!(fake.requests().len(), 1, "status {status} stops the walk");
        }
    }

    #[tokio::test]
    async fn a_two_hundred_only_answers_if_it_resolves_a_quota() {
        // A 2xx the extractor resolves is the answer.
        let fake = ScriptedHttp::new(vec![reply(200, CONVENTIONAL)]);
        let quota = fetch(&fake, QuotaAccount::new()).await.expect("answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(fake.requests().len(), 1);

        // A 2xx that resolves nothing is not an answer: the walk continues. The first body is the
        // shape that matters — a `remaining` with no `total`, which the extractor refuses because
        // there is no denominator.
        let fake = ScriptedHttp::new(vec![
            reply(200, r#"{"remaining":12}"#),
            reply(200, CONVENTIONAL),
        ]);
        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("second answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn headers_answer_only_when_no_body_did() {
        // No candidate serves a body, and one replies 200 with a limit header only.
        let mut replies = vec![reply_body_with(
            200,
            r#"{"hello":"world"}"#, // 2xx, nothing resolvable in the body
            &[("x-ratelimit-limit-requests", "500")],
        )];
        replies.extend((0..9).map(|_| reply(404, "{}")));
        let fake = ScriptedHttp::new(replies);

        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("headers answer");
        assert_eq!(quota.total, 500.0);
        assert!(
            !quota.fraction_reported,
            "a limit with no consumption is not a reported fraction"
        );
        assert_eq!(quota.reported_percentage(), None);
        assert_eq!(quota.remaining_fraction(), None);
        assert_eq!(
            quota.source.as_deref(),
            Some(format!("{} /v1/balance (headers)", OpenAiCompat::ID).as_str()),
            "the first candidate is the one that answered, and it answered in its headers"
        );

        // A header that only reports what is left, with no denominator, is not a quota. The rest of
        // the list is scripted too, because the walk goes on to the end looking for a body.
        let mut replies = vec![reply_with(
            200,
            &[("x-ratelimit-remaining-requests", "499")],
        )];
        replies.extend((0..9).map(|_| reply(404, "{}")));
        let fake = ScriptedHttp::new(replies);
        let error = fetch(&fake, QuotaAccount::new())
            .await
            .expect_err("no answer");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert_eq!(
            fake.requests().len(),
            9,
            "and every candidate was still tried"
        );

        // A zero limit is not a denominator either: never a `0/0` row.
        let mut replies = vec![reply_with(200, &[("x-ratelimit-limit-requests", "0")])];
        replies.extend((0..9).map(|_| reply(404, "{}")));
        let fake = ScriptedHttp::new(replies);
        let error = fetch(&fake, QuotaAccount::new())
            .await
            .expect_err("no answer");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");

        // A limit *and* what is left is a reported fraction.
        let mut replies = vec![reply_with(
            200,
            &[
                ("x-ratelimit-limit-requests", "500"),
                ("x-ratelimit-remaining-requests", "120"),
            ],
        )];
        replies.extend((0..9).map(|_| reply(404, "{}")));
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("headers answer");
        assert_eq!(
            (quota.total, quota.used, quota.remaining),
            (500.0, 380.0, 120.0)
        );
        assert!(quota.fraction_reported);
    }

    #[tokio::test]
    async fn a_balance_header_is_read_when_no_rate_limit_header_is() {
        let mut replies = vec![reply_with(
            200,
            &[("x-balance-total", "20"), ("x-balance-remaining", "3.5")],
        )];
        replies.extend((0..9).map(|_| reply(404, "{}")));
        let fake = ScriptedHttp::new(replies);

        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("headers answer");
        assert_eq!((quota.total, quota.remaining), (20.0, 3.5));
        assert!(quota.fraction_reported);
    }

    #[tokio::test]
    async fn the_one_token_post_is_the_last_candidate_and_needs_a_model() {
        // With a model configured, the last candidate is the POST, and it is read for its headers.
        let mut replies = unsupported();
        replies.remove(9); // the POST is the tenth request, so the script needs ten replies
        replies.push(reply_with(200, &[("x-ratelimit-limit-tokens", "200000")]));
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(
            &fake,
            account(json!({ "model": "the-model-this-account-uses" })),
        )
        .await
        .expect("the post answers");

        assert_eq!(quota.total, 200000.0);
        let requests = fake.requests();
        assert_eq!(requests.len(), 10);
        let last = requests.last().expect("ten requests");
        assert_eq!(last.method, http::Method::POST);
        assert_eq!(last.url, "https://api.example.test/v1/chat/completions");
        let body = String::from_utf8_lossy(&last.body).into_owned();
        assert!(
            body.contains("\"model\":"),
            "the probe names a model: {body}"
        );
        assert!(
            body.contains("the-model-this-account-uses"),
            "and the model is the configured one: {body}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method == http::Method::GET)
                .count(),
            9,
            "nine paths first"
        );

        // Without a model there is nothing to name, so the POST is not sent at all.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, QuotaAccount::new())
            .await
            .expect_err("no answer");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert_eq!(fake.requests().len(), 9);
        assert!(fake
            .requests()
            .iter()
            .all(|request| request.method == http::Method::GET));

        // And it can be switched off outright.
        let fake = ScriptedHttp::new(unsupported());
        let _ = fetch(
            &fake,
            account(json!({
                "model": "m",
                "chat_completion_probe": false,
            })),
        )
        .await;
        assert_eq!(fake.requests().len(), 9);
    }

    #[tokio::test]
    async fn every_request_stays_on_the_accounts_own_host() {
        // The list is the account's, and one entry names somebody else's host.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(
            &fake,
            account(json!(["https://evil.test/v1/balance", "/v1/balance"])),
        )
        .await
        .expect_err("refused");
        match &error {
            Error::Config(message) => {
                assert!(
                    message.contains("https://evil.test/v1/balance"),
                    "{message}"
                );
                assert!(message.contains("quotaProbe"), "{message}");
            }
            other => panic!("expected a configuration error, got {other:?}"),
        }
        assert!(
            fake.requests().is_empty(),
            "nothing is sent when a candidate names another host"
        );

        // The same, written protocol-relative.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, account(json!(["//evil.test/v1/balance"])))
            .await
            .expect_err("refused");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(fake.requests().is_empty());

        // And on a real walk, every request carries the account's own host.
        let mut replies = unsupported();
        replies[8] = reply(200, CONVENTIONAL);
        let fake = ScriptedHttp::new(replies);
        let quota = fetch(&fake, QuotaAccount::new()).await.expect("answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(fake.requests().len(), 9);
        for url in fake.urls() {
            let uri: Uri = url.parse().expect("a parsed url");
            assert_eq!(uri.host(), Some("api.example.test"), "{url}");
            assert_eq!(uri.scheme_str(), Some("https"), "{url}");
        }
    }

    #[tokio::test]
    async fn an_accounts_own_list_beats_the_configured_one() {
        let configured = QuotaProbeConfig {
            paths: vec!["/configured/one".to_owned()],
            ..QuotaProbeConfig::default()
        };
        let fake = ScriptedHttp::new(vec![reply(200, CONVENTIONAL)]);
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        )
        .with_account(account(json!(["/inline/two", "/inline/three"])))
        .with_base_url("https://api.example.test/v1")
        .with_quota_probe(configured.clone());

        let quota = walk(&ctx)
            .await
            .expect("the account's first candidate answers");
        assert_eq!(quota.total, 100.0);
        assert_eq!(fake.urls(), vec!["https://api.example.test/inline/two"]);

        // An object form may set one field and inherit the list.
        let fake = ScriptedHttp::new(vec![reply(200, CONVENTIONAL)]);
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        )
        .with_account(account(json!({ "enabled": true })))
        .with_base_url("https://api.example.test/v1")
        .with_quota_probe(configured);
        walk(&ctx).await.expect("inherits the configured list");
        assert_eq!(fake.urls(), vec!["https://api.example.test/configured/one"]);
    }

    #[tokio::test]
    async fn a_switched_off_walk_sends_nothing() {
        let fake = ScriptedHttp::new(unsupported());
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        )
        .with_account(QuotaAccount::new())
        .with_base_url("https://api.example.test/v1")
        .with_quota_probe(QuotaProbeConfig {
            enabled: false,
            ..QuotaProbeConfig::default()
        });

        let error = walk(&ctx).await.expect_err("nothing is tried");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(fake.requests().is_empty());

        // The account can switch itself off too.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, account(json!({ "enabled": false })))
            .await
            .expect_err("nothing is tried");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn an_unusable_declaration_is_reported_rather_than_walked() {
        // Not a list and not an object.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, account(json!("https://api.example.test/v1/balance")))
            .await
            .expect_err("refused");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(fake.requests().is_empty());

        // A path that is not a string.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, account(json!(["/v1/ok", 7])))
            .await
            .expect_err("refused");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(fake.requests().is_empty());

        // A model that is not a string.
        let fake = ScriptedHttp::new(unsupported());
        let error = fetch(&fake, account(json!({ "model": 7 })))
            .await
            .expect_err("refused");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");

        // The configured list is held to the same rule as the account's.
        let fake = ScriptedHttp::new(unsupported());
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        )
        .with_base_url("https://api.example.test/v1")
        .with_quota_probe(QuotaProbeConfig {
            paths: vec!["api.example.test/x".to_owned()],
            ..QuotaProbeConfig::default()
        });
        let error = walk(&ctx).await.expect_err("refused");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn a_long_list_is_bounded_and_a_missing_base_url_is_reported() {
        let paths: Vec<String> = (0..80).map(|n| format!("/v1/candidate-{n}")).collect();
        let fake = ScriptedHttp::new((0..MAX_PROBE_REQUESTS).map(|_| reply(404, "{}")).collect());
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        )
        .with_base_url("https://api.example.test/v1")
        .with_quota_probe(QuotaProbeConfig {
            paths,
            chat_completion_probe: false,
            ..QuotaProbeConfig::default()
        });
        let error = walk(&ctx).await.expect_err("nothing answers");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert_eq!(fake.requests().len(), MAX_PROBE_REQUESTS);

        // No base URL: there is no host to probe, and nothing is sent.
        let fake = ScriptedHttp::new(unsupported());
        let ctx = QuotaFetchContext::new(
            "openai-clone",
            Dialect::OpenAi,
            Arc::clone(&fake) as Arc<dyn QuotaHttp>,
        );
        let error = walk(&ctx).await.expect_err("no host");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn a_header_answer_is_kept_when_a_later_candidate_fails_hard() {
        let mut replies = vec![reply_with(200, &[("x-ratelimit-limit-requests", "500")])];
        replies.push(reply(500, "{}"));
        let fake = ScriptedHttp::new(replies);

        let quota = fetch(&fake, QuotaAccount::new())
            .await
            .expect("the header answer is not thrown away");
        assert_eq!(quota.total, 500.0);
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn the_walk_sends_the_accounts_credential_and_a_timeout() {
        let fake = ScriptedHttp::new(vec![reply(200, CONVENTIONAL)]);
        let _ = fetch(&fake, QuotaAccount::new()).await.expect("answers");
        let seen = fake.requests().remove(0);
        assert_eq!(seen.timeout, Some(QUOTA_ENDPOINT_TIMEOUT));
        assert!(seen.timeout != Some(Duration::ZERO));
        assert!(
            seen.headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer probe-secret"),
            "the credential is presented: {:?}",
            seen.headers
                .iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        );
    }
}

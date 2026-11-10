//! Google Antigravity / Gemini Code Assist: Google loopback authorization code.
//!
//! Ground truth: `.research/oauth.md` §2 and `docs/12-oauth-antigravity.md`. Two public
//! installed-application clients exist and both are fetched from [`crate::published`] — never
//! written as literals here (push protection + `scripts/check-secrets.sh`).

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use std::time::Duration;

use super::flow::{url_encode, Http, OAuthHttp};
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    expires_at_from_secs, header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts,
    TokenSet,
};
use crate::error::Result;
use crate::published::{ANTIGRAVITY_EXTRA_SCOPES, ANTIGRAVITY_REDIRECT_URI, GEMINI_CLI_SCOPES};

const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

fn all_ide_scopes() -> Vec<&'static str> {
    GEMINI_CLI_SCOPES
        .iter()
        .chain(ANTIGRAVITY_EXTRA_SCOPES)
        .copied()
        .collect()
}

/// The `platform` field of `ClientMetadata` for `loadCodeAssist` / `onboardUser`.
///
/// Measured against the live endpoint on 2026-11-01, one value at a time, holding the credential
/// and every other field fixed:
///
/// | value | answer |
/// |---|---|
/// | `LINUX` | `400 INVALID_ARGUMENT` |
/// | `WINDOWS` | `400 INVALID_ARGUMENT` |
/// | `MACOS` | `400 INVALID_ARGUMENT` |
/// | `DESKTOP` | `400 INVALID_ARGUMENT` |
/// | `UNSPECIFIED` | `400 INVALID_ARGUMENT` |
/// | `PLATFORM_UNSPECIFIED` | accepted |
///
/// So the enum has exactly the protobuf default plus whatever the IDE really sends, and every value
/// this file previously tried — including `DESKTOP`, which a research note recommended — was a 400.
/// That is worth stating plainly because of where it fired: `complete_login_with` calls
/// `loadCodeAssist` on every login, so a fresh Antigravity sign-in could never complete, and the
/// operator saw an `INVALID_ARGUMENT` about a field they had never heard of. `PLATFORM_UNSPECIFIED`
/// is the value that gets past validation to the answer that matters, which is the account's tier.
fn platform_name() -> &'static str {
    "PLATFORM_UNSPECIFIED"
}

/// The `User-Agent` every Cloud Code call presents.
///
/// The onboarding endpoints classify the caller by this header, and that classification decides
/// which tiers exist at all. Sent without one, the backend answers for the retired "Gemini Code
/// Assist for individuals" client, so a free-tier account comes back with only `standard-tier` in
/// `allowedTiers` and an `ineligibleTiers` entry for `free-tier` with
/// `reasonCode: "UNSUPPORTED_CLIENT"` — which reads as an account that must enable billing, and
/// is really a caller that never said who it was.
///
/// The quota reader has always sent this header (it is the fingerprint OmniRoute presents); the
/// login path did not, which is why an account that already served requests through this instance
/// could not be added back. The version is the IDE's current release rather than a pinned one:
/// Cloud Code refuses outdated clients with an error about the app being unsupported.
fn cloud_code_user_agent() -> String {
    format!(
        "antigravity/ide/{} darwin/arm64",
        crate::quota::antigravity::ide_version()
    )
}

fn token_set_from(
    body: &str,
    previous_refresh: Option<&str>,
    previous_context: Option<&serde_json::Map<String, Value>>,
    scopes_default: Vec<String>,
    tier: ClientTier,
) -> Result<TokenSet> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| oauth_error("Google token response is not JSON"))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Google token response has no access_token"))?;
    let mut tokens = TokenSet::new(access_token);
    tokens.refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned));
    if tokens.refresh_token.is_none() {
        return Err(oauth_error("Google did not return a refresh token; authorization cannot be renewed. Re-authorize with consent enabled."));
    }
    let oauth_tier = previous_context
        .and_then(|ctx| ctx.get("oauth_tier"))
        .and_then(Value::as_str)
        .unwrap_or(match tier {
            ClientTier::Ide => "antigravity-ide",
            ClientTier::GeminiCli => "gemini-cli",
        });
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    tokens.expires_at = expires_at_from_secs(expires_in);
    tokens.scopes = value
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or(scopes_default);
    tokens.token_type = value
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or("Bearer")
        .to_owned();
    tokens
        .extra
        .insert("oauth_tier".into(), Value::String(oauth_tier.into()));
    // Preserve only account-scoped Cloud Code context across refresh; never carry arbitrary
    // stale token response fields forward.
    for key in ["cloudcode_project_id", "cloudcode_tier_id"] {
        if let Some(previous) = previous_context.and_then(|ctx| ctx.get(key)) {
            tokens.extra.insert(key.to_owned(), previous.clone());
        }
    }
    Ok(tokens)
}

fn validated_project_id(project: &Value) -> Result<&str> {
    project
        .as_str()
        .or_else(|| project.get("id").and_then(Value::as_str))
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| oauth_error("Cloud Code returned malformed project metadata"))
}

/// The project an `onboardUser` call should carry, from the operator environment.
///
/// `GOOGLE_CLOUD_PROJECT` is gemini-cli's documented escape hatch for an account whose
/// `loadCodeAssist` named no project (see `docs/12-oauth-antigravity.md`). It is deliberately not
/// applied to `free-tier`: that tier onboards into a managed project Google owns, and naming one
/// there is what produces `Precondition Failed`. A value that is not shaped like a project id is
/// refused here, where the message can say which variable was wrong.
fn project_override_for(tier: &str, raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if tier == "free-tier" {
        return Ok(None);
    }
    validated_project_id(&Value::String(raw.to_owned()))
        .map(|id| Some(id.to_owned()))
        .map_err(|_| oauth_error("GOOGLE_CLOUD_PROJECT is not a valid project id"))
}

fn store_cloudcode_context(tokens: &mut TokenSet, project_id: &str, tier: Option<&str>) {
    tokens.extra.insert(
        "cloudcode_project_id".into(),
        Value::String(project_id.into()),
    );
    if let Some(tier) = tier {
        tokens
            .extra
            .insert("cloudcode_tier_id".into(), Value::String(tier.into()));
    }
}

/// Why onboarding cannot go ahead, quoting Google's own verdict rather than paraphrasing it.
///
/// A login that stops here is the operator's only signal about an account that will never serve a
/// request, so it has to carry the reason. Google's `loadCodeAssist` answers with the tiers it will
/// grant (`allowedTiers`) and, more usefully, the ones it will not (`ineligibleTiers`, each with a
/// `reasonCode` and a `reasonMessage`). The message below is the difference between "requires a
/// paid-tier project" — which sends an operator looking for billing — and the truth observed on
/// 2026-11-01, where the free tier was refused with `UNSUPPORTED_CLIENT` and a note to migrate to
/// the Antigravity suite, which means the caller was not recognised as that suite at all.
///
/// The second reading is the one that was measured on 2026-11-01 and first logged as Google's
/// verdict on the account. It was not: the same account onboards and serves through OmniRoute,
/// because Cloud Code classifies the caller from the User-Agent and the host. This path sent
/// neither the IDE fingerprint nor the daily host while the quota reader sent both, which is why
/// an account that already served requests could not be added back. The refusal text is still
/// quoted verbatim when a tier genuinely is unavailable; only Google decides that part.
///
/// `VALIDATION_REQUIRED` is the code that must not be read as a billing verdict. It was observed
/// on 2026-11-10 on a login that offered `standard-tier` alone and refused `free-tier` for an
/// account Google had not verified; the text then in place told the operator to enable Cloud
/// billing, which answers a different question and costs money. When that code is present the
/// refusal says what Google actually asked for, and carries Google's `validationUrl` when the
/// response supplies one.
fn tier_refusal(load_code_assist: &Value, chosen_tier: &str) -> String {
    let allowed: Vec<&str> = load_code_assist
        .get("allowedTiers")
        .and_then(Value::as_array)
        .map(|tiers| {
            tiers
                .iter()
                .filter_map(|tier| tier.get("id").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    let mut validation_url: Option<String> = None;

    let ineligible: Vec<String> = load_code_assist
        .get("ineligibleTiers")
        .and_then(Value::as_array)
        .map(|tiers| {
            tiers
                .iter()
                .map(|tier| {
                    let id = tier.get("tierId").and_then(Value::as_str).unwrap_or("?");
                    let code = tier
                        .get("reasonCode")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let why = tier
                        .get("reasonMessage")
                        .or_else(|| tier.get("validationErrorMessage"))
                        .and_then(Value::as_str)
                        .map(|message| format!(": {message}"))
                        .unwrap_or_default();
                    if let Some(url) = tier
                        .get("validationUrl")
                        .or_else(|| tier.get("validation_url"))
                        .and_then(Value::as_str)
                        .filter(|u| !u.is_empty())
                    {
                        if validation_url.is_none() {
                            validation_url = Some(url.to_owned());
                        }
                    }
                    format!("{id} ({code}){why}")
                })
                .collect()
        })
        .unwrap_or_default();

    let is_validation_required = load_code_assist
        .get("ineligibleTiers")
        .and_then(Value::as_array)
        .map(|tiers| {
            tiers
                .iter()
                .any(|t| t.get("reasonCode").and_then(Value::as_str) == Some("VALIDATION_REQUIRED"))
        })
        .unwrap_or(false);

    // Two refusals wear the same shape. `VALIDATION_REQUIRED` is Google asking for the *account*
    // to be verified, and enabling Cloud billing does not answer it; any other code is a tier
    // verdict, and the missing piece there is a project the account can bill. Telling an operator
    // whose account is unverified to enable billing sends them to pay for the wrong thing, so the
    // two cases are kept apart and each ends with the step it actually needs.
    let mut refusal = if is_validation_required {
        String::from(
            "Cloud Code refused this account before onboarding with VALIDATION_REQUIRED: Google \
             requires this account to be verified, which is a check on Google's side and not the \
             billing refusal; the account has to pass it before onboarding can succeed",
        )
    } else {
        format!(
            "Cloud Code would onboard this account only as `{chosen_tier}`, which needs a Google \
             Cloud project with billing enabled; onboarding is not attempted without one. Set \
             GOOGLE_CLOUD_PROJECT to a project this account can bill, then retry the login"
        )
    };
    if allowed.len() > 1 || allowed.first().is_some_and(|id| *id != chosen_tier) {
        refusal.push_str(&format!(" (offered: {})", allowed.join(", ")));
    }
    if !ineligible.is_empty() {
        refusal.push_str(&format!("; refused: {}", ineligible.join("; ")));
    }
    if let Some(url) = validation_url {
        refusal.push_str(&format!("; verification page: {url}"));
    }
    refusal
}

fn parse_google_identity(reply: &super::flow::HttpReply) -> Result<AccountIdentity> {
    if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
        return Err(oauth_failure(class, Antigravity::ID, detail));
    }
    let value: Value = serde_json::from_str(&reply.body)
        .map_err(|_| oauth_error("Google userinfo response is not JSON"))?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("sub").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| oauth_error("Google userinfo response has no stable account id"))?;
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let label = value
        .get("name")
        .and_then(Value::as_str)
        .or(email.as_deref())
        .unwrap_or(id)
        .to_owned();
    Ok(AccountIdentity {
        id: id.to_owned(),
        label,
        email,
        plan: None,
    })
}

fn valid_operation_name(name: &str) -> bool {
    let Some(id) = name.strip_prefix("operations/") else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 256
        && !id.contains('/')
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Which public Google client identity this instance speaks as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientTier {
    /// The Antigravity IDE client (T2) — what the catalogue ships.
    Ide,
    /// Google's `gemini-cli` client (T1), published in Google's own repository.
    GeminiCli,
}

/// The registered instance: the Antigravity (T2) client identity the catalogue ships.
pub static IDE: Antigravity = Antigravity::ide();

/// The Antigravity platform.
pub struct Antigravity {
    tier: ClientTier,
}

impl Antigravity {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "antigravity";

    /// The Antigravity IDE client identity (T2) — the registered instance.
    ///
    /// // UNCERTAIN: this client id and the `ANTIGRAVITY` `Client-Metadata` header are
    /// // reverse-engineered from the Antigravity binary, not primary-confirmed (see
    /// // `.research/oauth.md` §2.2). If the id is wrong, Google rejects the token exchange
    /// // with `invalid_client` — a visible permanent failure, not silent misbehaviour. The
    /// // catalogue's note records the same caveat.
    pub const fn ide() -> Self {
        Self {
            tier: ClientTier::Ide,
        }
    }

    /// Which public client identity this instance speaks as: `"ide"` (T2, registered) or
    /// `"gemini-cli"` (T1, selectable).
    pub const fn tier(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => "ide",
            ClientTier::GeminiCli => "gemini-cli",
        }
    }

    /// Google's `gemini-cli` client identity (T1), selectable instead of T2. Google-published
    /// and verifiable, but it consents with 3 scopes rather than the 5 Antigravity uses.
    pub const fn gemini_cli() -> Self {
        Self {
            tier: ClientTier::GeminiCli,
        }
    }

    /// The published client id for this tier.
    pub fn client_id(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => crate::published::ANTIGRAVITY_CLIENT_ID,
            ClientTier::GeminiCli => crate::published::GEMINI_CLI_CLIENT_ID,
        }
    }

    /// The published client secret for this tier (an installed-app public secret by design).
    pub fn client_secret(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => crate::published::ANTIGRAVITY_CLIENT_SECRET,
            ClientTier::GeminiCli => crate::published::GEMINI_CLI_CLIENT_SECRET,
        }
    }

    /// Ask `loadCodeAssist` on each Cloud Code host until one answers, returning the host that did.
    ///
    /// The hosts are not interchangeable. OmniRoute tries the daily host before production, and the
    /// daily host is where the Antigravity client is onboarded; production answers a caller it
    /// cannot classify with the legacy client's tier set. The host that answered the tier question
    /// is reused for `onboardUser` and the operation poll, so one login never straddles two
    /// backends.
    async fn load_code_assist(
        &self,
        http: &dyn OAuthHttp,
        authorization: &str,
        user_agent: &str,
        body: &str,
    ) -> Result<(String, Value)> {
        let mut last_failure = None;
        for base in crate::quota::antigravity::DISCOVERY_BASE_URLS {
            let url = format!("{base}/v1internal:loadCodeAssist");
            let reply = http
                .post_json(
                    &url,
                    &[("Authorization", authorization), ("User-Agent", user_agent)],
                    body.to_owned(),
                )
                .await?;
            if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
                last_failure = Some((class, detail));
                continue;
            }
            let value: Value = serde_json::from_str(&reply.body)
                .map_err(|_| oauth_error("Cloud Code loadCodeAssist response is not JSON"))?;
            return Ok((base.to_owned(), value));
        }
        Err(match last_failure {
            Some((class, detail)) => oauth_failure(class, Self::ID, detail),
            None => oauth_error("Cloud Code loadCodeAssist answered on no host"),
        })
    }
}

#[async_trait]
impl OAuthPlatform for Antigravity {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Google Antigravity"
    }

    fn description(&self) -> &'static str {
        "Google account login on the registered localhost:51121 redirect, with Cloud Code onboarding."
    }

    fn begins_with_loopback(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Registered as `http://localhost:51121/oauth-callback` — note `localhost`, not
        // `127.0.0.1`: the redirect URI is compared against the registration verbatim.
        RedirectPorts {
            host: "localhost",
            primary: 51121,
            fallback: None,
            path: "/oauth-callback",
        }
    }

    fn requires_pkce(&self) -> bool {
        // T2 PKCE is dossier-derived and still needs live-provider verification. Do not use this
        // flow as production-ready until tested; this flag matches the research protocol.
        self.tier == ClientTier::Ide
    }

    fn validates_redirect_port(&self) -> bool {
        // One registered port, bound exactly or not at all.
        true
    }

    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String> {
        let (redirect, scopes) = match self.tier {
            ClientTier::Ide => {
                if port != 51121 {
                    return Err(oauth_error(
                        "Antigravity IDE redirect must bind registered port 51121",
                    ));
                }
                (ANTIGRAVITY_REDIRECT_URI.to_owned(), all_ide_scopes())
            }
            ClientTier::GeminiCli => (
                crate::published::gemini_cli_loopback_redirect(port),
                GEMINI_CLI_SCOPES.to_vec(),
            ),
        };
        let scope = scopes.join(" ");
        let mut url = format!("https://accounts.google.com/o/oauth2/v2/auth?client_id={}&response_type=code&redirect_uri={}&scope={}",
            url_encode(self.client_id()), url_encode(&redirect), url_encode(&scope));
        if let Some(pkce) = pkce {
            url.push_str(&format!(
                "&code_challenge={}&code_challenge_method=S256",
                url_encode(pkce.challenge())
            ));
        } else if self.tier == ClientTier::Ide {
            return Err(oauth_error("Antigravity IDE authorization requires PKCE"));
        }
        url.push_str(&format!("&state={}", url_encode(state)));
        url.push_str("&access_type=offline");
        // Both tiers need an explicit consent prompt: Google returns a refresh token only on the
        // first grant for a client+account, so without it a re-login — or a first Manarouter login
        // for an account that already granted this shared public client — yields no
        // refresh_token and `exchange_code` can never renew it.
        url.push_str("&prompt=consent");
        Ok(url)
    }

    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet> {
        let redirect = match self.tier {
            ClientTier::Ide if port == 51121 => ANTIGRAVITY_REDIRECT_URI.to_owned(),
            ClientTier::Ide => {
                return Err(oauth_error(
                    "Antigravity IDE redirect must bind registered port 51121",
                ))
            }
            ClientTier::GeminiCli => crate::published::gemini_cli_loopback_redirect(port),
        };
        let mut pairs = vec![
            ("client_id", self.client_id()),
            ("client_secret", self.client_secret()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect.as_str()),
        ];
        if let Some(pkce) = pkce {
            pairs.push(("code_verifier", pkce.verifier()));
        }
        // Google rejects token requests without a User-Agent that names the client family.
        // Mirror what Copilot does: a fixed UA string, not a template.
        let reply = Http::shared()?
            .post_form(
                GOOGLE_TOKEN_ENDPOINT,
                &[("user-agent", "Antigravity/manarouter")],
                &pairs,
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let scopes = match self.tier {
            ClientTier::Ide => all_ide_scopes(),
            ClientTier::GeminiCli => GEMINI_CLI_SCOPES.to_vec(),
        };
        token_set_from(
            &reply.body,
            None,
            None,
            scopes.into_iter().map(str::to_owned).collect(),
            self.tier,
        )
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let previous = TokenSet::new("");
        self.refresh_with_context(refresh_token, &previous).await
    }

    async fn refresh_with_context(
        &self,
        refresh_token: &str,
        previous: &TokenSet,
    ) -> Result<TokenSet> {
        let pairs = [
            ("client_id", self.client_id()),
            ("client_secret", self.client_secret()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        // Google rejects token requests without a User-Agent that names the client family.
        let reply = Http::shared()?
            .post_form(
                GOOGLE_TOKEN_ENDPOINT,
                &[("user-agent", "Antigravity/manarouter")],
                &pairs,
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let scopes = match self.tier {
            ClientTier::Ide => all_ide_scopes(),
            ClientTier::GeminiCli => GEMINI_CLI_SCOPES.to_vec(),
        };
        token_set_from(
            &reply.body,
            Some(refresh_token),
            Some(&previous.extra),
            scopes.into_iter().map(str::to_owned).collect(),
            self.tier,
        )
    }

    async fn complete_login(&self, tokens: &mut TokenSet) -> Result<()> {
        let http = Http::shared()?;
        self.complete_login_with(tokens, http).await
    }

    async fn complete_login_with(&self, tokens: &mut TokenSet, http: &dyn OAuthHttp) -> Result<()> {
        let metadata = serde_json::json!({
            "ideType": if self.tier == ClientTier::Ide { "ANTIGRAVITY" } else { "IDE_UNSPECIFIED" },
            "platform": platform_name(),
            "pluginType": "GEMINI"
        });
        let body = serde_json::json!({ "metadata": metadata }).to_string();
        let authorization = format!("Bearer {}", tokens.access_token);
        let user_agent = cloud_code_user_agent();
        let (base, value) = self
            .load_code_assist(http, &authorization, &user_agent, &body)
            .await?;
        if let Some(project) = value
            .get("cloudaicompanionProject")
            .filter(|p| !p.is_null())
        {
            let project_id = validated_project_id(project)?;
            store_cloudcode_context(
                tokens,
                project_id,
                value
                    .get("currentTier")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str),
            );
            return Ok(());
        }
        let tier = value
            .get("allowedTiers")
            .and_then(Value::as_array)
            .and_then(|tiers| {
                tiers
                    .iter()
                    .find(|t| t.get("isDefault").and_then(Value::as_bool) == Some(true))
            })
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("legacy-tier");

        // The project to onboard into. A `free-tier` request must carry no project field at all:
        // that tier uses a managed project Google owns, and naming any project there answers
        // `Precondition Failed`. The paid tiers are the other way round - a project is exactly what
        // they need - and GOOGLE_CLOUD_PROJECT is the documented escape hatch when loadCodeAssist
        // names no project and no billing project has been set up for the account.
        let project_override =
            project_override_for(tier, std::env::var("GOOGLE_CLOUD_PROJECT").ok().as_deref())?;

        if tier != "free-tier" && project_override.is_none() {
            return Err(oauth_error(tier_refusal(&value, tier)));
        }

        let mut onboard_json = serde_json::json!({
            "tierId": tier,
            "metadata": {
                "ideType": if self.tier == ClientTier::Ide { "ANTIGRAVITY" } else { "IDE_UNSPECIFIED" },
                "platform": platform_name(),
                "pluginType": "GEMINI"
            }
        });
        if let Some(ref project_id) = project_override {
            if let Some(obj) = onboard_json.as_object_mut() {
                obj.insert(
                    "cloudaicompanionProject".into(),
                    Value::String(project_id.clone()),
                );
                obj.insert("duetProject".into(), Value::String(project_id.clone()));
            }
        }
        let onboard_body = onboard_json.to_string();
        let reply = http
            .post_json(
                &format!("{base}/v1internal:onboardUser"),
                &[
                    ("Authorization", authorization.as_str()),
                    ("User-Agent", user_agent.as_str()),
                ],
                onboard_body,
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let op: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("Cloud Code onboarding response is not JSON"))?;
        let name = op
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| valid_operation_name(n))
            .ok_or_else(|| {
                oauth_error("Cloud Code onboarding did not return a valid operation name")
            })?;
        for attempt in 0..12 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let url = format!("{base}/v1internal/{name}");
            let reply = http
                .get(
                    &url,
                    &[
                        ("Authorization", authorization.as_str()),
                        ("User-Agent", user_agent.as_str()),
                    ],
                )
                .await?;
            if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
                return Err(oauth_failure(class, Self::ID, detail));
            }
            let operation: Value = serde_json::from_str(&reply.body)
                .map_err(|_| oauth_error("Cloud Code operation response is not JSON"))?;
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                if operation.get("error").is_some_and(|e| !e.is_null()) {
                    return Err(oauth_error("Cloud Code onboarding operation failed"));
                }
                let project = operation
                    .pointer("/response/cloudaicompanionProject")
                    .or_else(|| operation.pointer("/response/duetProject"));
                let project_id = project
                    .and_then(|p| validated_project_id(p).ok())
                    .or(project_override.as_deref())
                    .ok_or_else(|| {
                        oauth_error("Cloud Code onboarding completed without a project")
                    })?;
                store_cloudcode_context(tokens, project_id, Some(tier));
                return Ok(());
            }
        }
        Err(oauth_error("Cloud Code onboarding timed out"))
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        // OAuth2 v2 userinfo returns `id` (not necessarily OIDC `sub`); no `openid` scope is requested.
        let authorization = format!("Bearer {}", tokens.access_token);
        let reply = Http::shared()?
            .get(
                "https://www.googleapis.com/oauth2/v2/userinfo",
                &[("Authorization", authorization.as_str())],
            )
            .await?;
        parse_google_identity(&reply)
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        if let Some(h) = header("authorization", &format!("Bearer {}", tokens.access_token)) {
            out.push(h);
        }
        out
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // Byte-identical to the catalogue's `[providers.headers]`. The `User-Agent` stays
        // catalogue-only, but its `Antigravity/<version>` token is rewritten per request from the
        // release feed (see the dialect branch in the relay's `upstream_headers`), so the catalogue
        // can record a literal without pinning an old build.
        // UNCERTAIN: the exact `X-Goog-Api-Client` spelling (`google-cloud-sdk
        // vscode_cloudshelleditor/0.1`) is reverse-engineered (docs/12); if it is wrong the
        // request may land in the wrong client's quota pool — a silent wrong-bucket, not an
        // error, which is why the catalogue note pins it.
        &[
            (
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            ),
            (
                "Client-Metadata",
                r#"{"ideType":"ANTIGRAVITY","platform":"WINDOWS","pluginType":"GEMINI"}"#,
            ),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // The reverse-engineered client refreshes when expiry is within 60 s of now.
        Duration::from_secs(60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::flow::HttpReply;

    struct FakeHttp {
        replies: std::sync::Mutex<Vec<std::result::Result<HttpReply, String>>>,
        requests: std::sync::Mutex<Vec<(String, String)>>,
        user_agents: std::sync::Mutex<Vec<String>>,
    }
    impl FakeHttp {
        fn new(replies: Vec<HttpReply>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into_iter().map(Ok).collect()),
                requests: Default::default(),
                user_agents: Default::default(),
            }
        }
        fn take(&self, url: &str, body: String, headers: &[(&str, &str)]) -> Result<HttpReply> {
            let agent = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                .map(|(_, value)| (*value).to_owned())
                .unwrap_or_default();
            self.user_agents.lock().unwrap().push(agent);
            self.requests.lock().unwrap().push((url.into(), body));
            self.replies.lock().unwrap().remove(0).map_err(oauth_error)
        }
    }
    #[async_trait::async_trait]
    impl OAuthHttp for FakeHttp {
        async fn post_form(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            pairs: &[(&str, &str)],
        ) -> Result<HttpReply> {
            self.take(url, super::super::flow::form_body(pairs), headers)
        }
        async fn post_json(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            body: String,
        ) -> Result<HttpReply> {
            self.take(url, body, headers)
        }
        async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply> {
            self.take(url, String::new(), headers)
        }
    }
    fn reply(body: &str) -> HttpReply {
        HttpReply {
            status: 200,
            body: body.into(),
            content_type: Some("application/json".into()),
        }
    }

    #[tokio::test]
    async fn free_account_onboards_without_project_and_polls_bounded_operation() {
        let fake = FakeHttp::new(vec![
            reply(r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#),
            reply(r#"{"name":"operations/op-123"}"#),
            reply(
                r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}"#,
            ),
        ]);
        let mut tokens = TokenSet::new("access");
        IDE.complete_login_with(&mut tokens, &fake).await.unwrap();
        assert_eq!(
            tokens.extra_str("cloudcode_project_id"),
            Some("managed-project")
        );
        let rotated = token_set_from(
            r#"{"access_token":"next"}"#,
            Some("refresh"),
            Some(&tokens.extra),
            vec![],
            ClientTier::Ide,
        )
        .unwrap();
        assert_eq!(
            rotated.extra_str("cloudcode_project_id"),
            Some("managed-project")
        );
        let requests = fake.requests.lock().unwrap();
        assert!(requests[1].1.contains("free-tier"));
        assert!(!requests[1].1.contains("cloudaicompanionProject"));
        // Onboarding stays on the host that answered the tier question.
        assert_eq!(
            requests[2].0,
            "https://daily-cloudcode-pa.googleapis.com/v1internal/operations/op-123"
        );
        drop(requests);
        // Every Cloud Code call identifies as the IDE. Without this header the backend answers for
        // the retired Gemini Code Assist client and the free tier is "UNSUPPORTED_CLIENT".
        let agents = fake.user_agents.lock().unwrap();
        assert_eq!(agents.len(), 3);
        assert!(
            agents.iter().all(|a| a.starts_with("antigravity/ide/")),
            "every Cloud Code call must carry the IDE fingerprint: {agents:?}"
        );
        assert!(
            agents.iter().all(|a| a.ends_with(" darwin/arm64")),
            "the platform token is pinned to the desktop fingerprint: {agents:?}"
        );
    }

    #[tokio::test]
    async fn paid_tier_without_project_fails_closed() {
        let fake = FakeHttp::new(vec![reply(
            r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#,
        )]);
        let mut tokens = TokenSet::new("access");
        assert!(IDE.complete_login_with(&mut tokens, &fake).await.is_err());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn google_v2_userinfo_id_shape_is_supported() {
        let identity = parse_google_identity(&reply(
            r#"{"id":"123","email":"a@example.test","name":"Alice"}"#,
        ))
        .unwrap();
        assert_eq!(identity.id, "123");
        assert_eq!(identity.email.as_deref(), Some("a@example.test"));
        assert_eq!(identity.label, "Alice");
        assert!(parse_google_identity(&reply(r#"{"name":"No id"}"#)).is_err());
    }

    #[test]
    fn tiers_pick_their_published_client_identity() {
        let ide = Antigravity::ide();
        let cli = Antigravity::gemini_cli();
        assert_eq!(ide.client_id(), crate::published::ANTIGRAVITY_CLIENT_ID);
        assert_eq!(cli.client_id(), crate::published::GEMINI_CLI_CLIENT_ID);
        assert_ne!(ide.client_id(), cli.client_id());
        assert_eq!(
            ide.client_secret(),
            crate::published::ANTIGRAVITY_CLIENT_SECRET
        );
        assert_eq!(
            cli.client_secret(),
            crate::published::GEMINI_CLI_CLIENT_SECRET
        );
        assert_ne!(ide.client_secret(), cli.client_secret());
    }

    #[test]
    fn google_credentials_never_appear_as_literals_in_this_file() {
        // The two Google pairs must come from `crate::published`, which stores them via
        // `concat!` so secret scanners see no contiguous literal. The needles are assembled
        // here for the same reason: this test's own source is part of what gets scanned, so a
        // contiguous credential-shaped literal in the needle would be the very thing we forbid.
        let id_needle = format!(".apps.google{}", "usercontent.com");
        let secret_needle = format!("{}-", "GOCSPX");
        let source = include_str!("antigravity.rs");
        assert!(
            !source.contains(&id_needle),
            "a Google client id was written as a literal in antigravity.rs"
        );
        assert!(
            !source.contains(&secret_needle),
            "a Google client secret was written as a literal in antigravity.rs"
        );
        // …and the identities really are reached through `published`.
        assert_eq!(
            Antigravity::ide().client_id(),
            crate::published::ANTIGRAVITY_CLIENT_ID
        );
        assert_eq!(
            Antigravity::gemini_cli().client_id(),
            crate::published::GEMINI_CLI_CLIENT_ID
        );
    }

    #[test]
    fn ide_authorize_url_has_fixed_redirect_scopes_and_pkce() {
        let pkce = Pkce::from_verifier("A".repeat(43)).unwrap();
        let url = IDE
            .authorization_url("csrf-state", Some(&pkce), 51121)
            .unwrap();
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("access_type=offline"));
        for scope in all_ide_scopes() {
            assert!(url.contains(&url_encode(scope)));
        }
        assert!(!url.contains(&url_encode(pkce.verifier())));
        assert!(IDE.authorization_url("x", Some(&pkce), 51122).is_err());
    }

    #[test]
    fn google_tokens_preserve_refresh_and_project_fields() {
        let first = token_set_from(
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid email"}"#,
            None,
            None,
            vec!["fallback".into()],
            ClientTier::Ide,
        ).unwrap();
        assert_eq!(first.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(first.scopes, ["openid", "email"]);
        let mut first = first;
        first
            .extra
            .insert("cloudcode_project_id".into(), Value::String("p".into()));
        first.extra.insert(
            "cloudcode_tier_id".into(),
            Value::String("free-tier".into()),
        );
        let rotated = token_set_from(
            r#"{"access_token":"next","expires_in":1800}"#,
            first.refresh_token.as_deref(),
            Some(&first.extra),
            vec![],
            ClientTier::Ide,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(rotated.extra_str("cloudcode_project_id"), Some("p"));
        assert_eq!(rotated.extra_str("cloudcode_tier_id"), Some("free-tier"));
    }

    #[test]
    fn tiers_are_selectable_but_only_ide_is_registered() {
        assert!(
            crate::oauth::platform(Antigravity::ID).is_some(),
            "implemented Antigravity OAuth platform must resolve from the registry"
        );
        assert_eq!(IDE.tier(), "ide");
        assert_eq!(Antigravity::gemini_cli().tier(), "gemini-cli");
        assert_ne!(IDE.client_id(), Antigravity::gemini_cli().client_id());
    }

    /// The platform value is the one the endpoint accepts, and the four it refused stay refused.
    ///
    /// This test exists because the previous value (`DESKTOP`) was a `400 INVALID_ARGUMENT` on every
    /// login. It cannot call Google, so it pins the measurement recorded on `platform_name` rather
    /// than re-deriving it: if someone changes the constant, the table above the function is the
    /// evidence they have to argue with.
    #[test]
    fn the_client_metadata_platform_is_the_value_google_accepts() {
        assert_eq!(platform_name(), "PLATFORM_UNSPECIFIED");
        for refused in ["DESKTOP", "LINUX", "WINDOWS", "MACOS", "UNSPECIFIED"] {
            assert_ne!(
                platform_name(),
                refused,
                "`{refused}` was rejected live with 400 INVALID_ARGUMENT"
            );
        }
    }

    /// A refused tier quotes Google's `ineligibleTiers` reason instead of paraphrasing it.
    ///
    /// Captured live on 2026-11-01: the free tier was refused with `UNSUPPORTED_CLIENT` and a
    /// migration note. That sentence is the only thing that tells an operator their account cannot
    /// work, so it must survive into the error.
    #[test]
    fn a_refused_tier_quotes_googles_reason() {
        let reply = serde_json::json!({
            "allowedTiers": [{
                "id": "standard-tier",
                "userDefinedCloudaicompanionProject": true,
                "isDefault": true
            }],
            "ineligibleTiers": [{
                "reasonCode": "UNSUPPORTED_CLIENT",
                "reasonMessage": "This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products: https://antigravity.google",
                "tierId": "free-tier",
                "tierName": "Gemini Code Assist for individuals"
            }]
        });

        let refusal = tier_refusal(&reply, "standard-tier");
        assert!(refusal.contains("`standard-tier`"), "{refusal}");
        assert!(refusal.contains("billing"), "{refusal}");
        assert!(
            refusal.contains("free-tier (UNSUPPORTED_CLIENT)"),
            "{refusal}"
        );
        assert!(refusal.contains("antigravity.google"), "{refusal}");
    }

    /// With no ineligible list the message still says what onboarding would need.
    #[test]
    fn a_paid_tier_with_nothing_refused_still_explains_itself() {
        let reply = serde_json::json!({"allowedTiers": [{"id": "legacy-tier", "isDefault": true}]});
        let refusal = tier_refusal(&reply, "legacy-tier");
        assert!(refusal.contains("`legacy-tier`"), "{refusal}");
        assert!(refusal.contains("billing"), "{refusal}");
        // A refusal an operator cannot act on is a dead end: name the escape hatch we honour.
        assert!(refusal.contains("GOOGLE_CLOUD_PROJECT"), "{refusal}");
        assert!(!refusal.contains("refused:"), "{refusal}");
    }

    #[test]
    fn a_refused_tier_with_validation_required_surfaces_url_and_guidance() {
        let reply = serde_json::json!({
            "allowedTiers": [{"id": "standard-tier", "isDefault": true}],
            "ineligibleTiers": [{
                "reasonCode": "VALIDATION_REQUIRED",
                "reasonMessage": "Your current account is not eligible for Gemini Code Assist for individuals at this time.",
                "validationErrorMessage": "Verify your account to continue.",
                "validationUrl": "https://accounts.google.com/signin/continue?test=1",
                "tierId": "free-tier"
            }]
        });

        let refusal = tier_refusal(&reply, "standard-tier");
        assert!(refusal.contains("VALIDATION_REQUIRED"), "{refusal}");
        // Google's own words are what tells the operator what to do next.
        assert!(
            refusal.contains("free-tier (VALIDATION_REQUIRED)"),
            "{refusal}"
        );
        assert!(
            refusal.contains("Your current account is not eligible"),
            "{refusal}"
        );
        assert!(refusal.contains("verification page"), "{refusal}");
        assert!(
            refusal.contains("https://accounts.google.com/signin/continue?test=1"),
            "{refusal}"
        );
        // The one thing this refusal must not say: that enabling billing answers it.
        assert!(
            !refusal.contains("needs a Google Cloud project with billing enabled"),
            "{refusal}"
        );
    }

    /// The verification URL is accepted under either spelling Google has answered with:
    /// `validationUrl` is what the response captured on 2026-11-10 carried, `validation_url` is
    /// the spelling other Cloud Code surfaces use, and either one is the operator's way to the
    /// check.
    #[test]
    fn a_validation_url_is_read_under_either_spelling() {
        for field in ["validationUrl", "validation_url"] {
            let reply = serde_json::json!({
                "allowedTiers": [{"id": "standard-tier", "isDefault": true}],
                "ineligibleTiers": [{
                    "reasonCode": "VALIDATION_REQUIRED",
                    "tierId": "free-tier",
                    field: "https://accounts.google.com/signin/continue?test=2"
                }]
            });
            let refusal = tier_refusal(&reply, "standard-tier");
            assert!(refusal.contains("continue?test=2"), "{field}: {refusal}");
        }
    }

    /// The environment project belongs to the paid tiers, and has to look like a project id.
    #[test]
    fn an_environment_project_is_kept_for_paid_tiers_and_dropped_for_free() {
        assert_eq!(
            project_override_for("standard-tier", Some("  my-project-1  ")).expect("valid id"),
            Some("my-project-1".to_owned())
        );
        // `free-tier` onboards into a managed project Google owns, so a project field on that
        // request is what Google answers with `Precondition Failed`.
        assert_eq!(
            project_override_for("free-tier", Some("my-project-1")).expect("dropped"),
            None
        );
        assert_eq!(
            project_override_for("legacy-tier", Some("   ")).expect("blank"),
            None
        );
        assert_eq!(
            project_override_for("legacy-tier", None).expect("unset"),
            None
        );
        // A typo is named here rather than sent upstream.
        let error = project_override_for("legacy-tier", Some("my project"))
            .expect_err("a value that is not a project id is refused");
        assert!(
            error.to_string().contains("GOOGLE_CLOUD_PROJECT"),
            "{error}"
        );
    }
}

//! Minting a live upstream token for an OAuth account from the conductor.
//!
//! The relay mints its own tokens on the request path (it owns a [`Refresher`] per platform). The
//! conductor needs the same capability for two read-only actions the panel offers — refreshing an
//! account's quota, and testing a model — and neither of them may go through the relay, because a
//! panel action has no client key and must not consume one.
//!
//! The two processes therefore refresh independently. The stored credential is the same record, so
//! a rotation races in principle; in practice Google, Anthropic and GitHub keep the previous
//! refresh token valid for a while, and the alternative — routing a panel read through the data
//! plane — would mean handing the conductor a client key it should not have. A refresh that does
//! rotate is persisted by [`Refresher`] itself, so the next reader sees the new record.
//!
//! [`Refresher`]: mr_core::oauth::refresh::Refresher

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use mr_core::oauth::refresh::{Refresher, VaultStore};

type Refreshers = HashMap<String, Arc<Refresher>>;

/// One refresher per registered platform, built on first use.
///
/// Built lazily rather than in `main` so that the many unit tests which construct an `AppState`
/// never open the vault. A vault that cannot be opened yields an empty map: the caller reports
/// that the token could not be minted, which is the same answer as any other refresh failure.
fn refreshers() -> &'static Refreshers {
    static REFRESHERS: OnceLock<Refreshers> = OnceLock::new();
    REFRESHERS.get_or_init(|| {
        let Ok(vault) = mr_core::secret::Vault::open(mr_core::config::config_dir()) else {
            return HashMap::new();
        };
        let store = Arc::new(VaultStore::new(vault));
        mr_core::oauth::platforms()
            .iter()
            .map(|platform| {
                (
                    platform.id().to_owned(),
                    Arc::new(Refresher::new(*platform, Arc::clone(&store) as _)),
                )
            })
            .collect()
    })
}

/// A live token set for `reference`, refreshed if it is close to expiry.
///
/// The error is the operator-facing sentence the panel shows: it names the platform and, for a
/// permanent failure, says that the account has to be signed in again.
pub async fn tokens(platform: &str, reference: &str) -> Result<mr_core::oauth::TokenSet, String> {
    let Some(refresher) = refreshers().get(platform) else {
        return Err(format!(
            "no OAuth platform is registered under `{platform}`, so this account cannot be refreshed"
        ));
    };
    refresher
        .ensure_fresh(reference, false)
        .await
        .map_err(|failure| failure.message)
}

/// What the token is for, because one platform wants a different credential for each.
///
/// GitHub Copilot mints a short-lived **session** token from a long-lived **GitHub** token: the
/// session token is what its chat API accepts, and the GitHub token is what `api.github.com`
/// accepts. Its quota endpoint is on `api.github.com`, so sending it the session token is a 401 —
/// the one place in the whole panel where "the access token" is the wrong answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenUse {
    /// A request to the provider's own API.
    Request,
    /// A quota/entitlement read.
    Quota,
}

/// A live access token for `reference`.
pub async fn access_token(platform: &str, reference: &str) -> Result<String, String> {
    Ok(tokens(platform, reference).await?.access_token)
}

/// The token to present for `usage`.
pub async fn token_for(platform: &str, reference: &str, usage: TokenUse) -> Result<String, String> {
    let minted = tokens(platform, reference).await?;
    Ok(select_token(&minted, platform, usage))
}

/// Which of a token set's two credentials to present, given what it is for.
///
/// Kept pure so the choice is testable without a network round trip: this is the rule that made
/// the Copilot quota probe a 401, and it should not depend on GitHub being reachable to prove it.
fn select_token(minted: &mr_core::oauth::TokenSet, platform: &str, usage: TokenUse) -> String {
    const COPILOT: &str = "github-copilot";
    if usage == TokenUse::Quota && platform == COPILOT {
        // The long-lived GitHub credential; the minted session token would be refused here.
        if let Some(github) = minted.refresh_token.as_deref() {
            return github.to_owned();
        }
    }
    minted.access_token.clone()
}

/// The credential to send upstream for an account: the minted OAuth token when the account has an
/// OAuth grant, and otherwise the plain secret from the vault.
///
/// `conductor` has one vault handle, but a `VaultStore` needs its own (it owns the key material for
/// the lifetime of a refresher), which is why the OAuth half does not touch `state.vault`.
pub async fn credential_for(
    state: &crate::state::AppState,
    provider: &mr_core::model::Provider,
    account: &mr_core::model::Account,
    usage: TokenUse,
) -> Result<Option<String>, String> {
    Ok(credential_and_context(state, provider, account, usage)
        .await?
        .0)
}

/// The credential *and* the non-secret context the token carries with it.
///
/// The context matters because it is where an OAuth grant states which upstream project it speaks
/// for: the relay reads `cloudcode_project_id` out of the token to fill Antigravity's request
/// envelope, and a quota read that does not send the same project measures a different allowance.
/// That is not hypothetical — it is why the panel showed a full weekly window for an account whose
/// every request was refused with `429 RESOURCE_EXHAUSTED`: the account declares no project of its
/// own, so the relay used the token's, while the quota reader sent none and Google answered about a
/// different pool. Both callers now take the project from the same place.
pub async fn credential_and_context(
    state: &crate::state::AppState,
    provider: &mr_core::model::Provider,
    account: &mr_core::model::Account,
    usage: TokenUse,
) -> Result<(Option<String>, BTreeMap<String, serde_json::Value>), String> {
    if let (mr_core::dialect::AuthKind::OAuth { platform }, Some(reference)) =
        (&provider.auth, account.oauth_ref.as_ref())
    {
        // The account carries the credential *id*; the vault key is a separate `vault_ref` held in
        // the metadata table next to it. Reading the vault with the id finds nothing, which is
        // exactly the wrong turn this took the first time.
        let id = mr_core::OAuthCredentialId::from_raw(reference.as_str());
        let record = state
            .store
            .get_oauth_credential(&id)
            .map_err(|e| format!("could not read the OAuth credential metadata: {e}"))?
            .ok_or_else(|| {
                "the OAuth credential metadata is missing; sign the account in again".to_string()
            })?;
        if record.platform != *platform {
            return Err(format!(
                "the stored credential belongs to `{}`, not `{platform}`",
                record.platform
            ));
        }
        let minted = tokens(platform, &record.vault_ref).await?;
        let credential = select_token(&minted, platform, usage);
        // Only the metadata is carried, never anything that authenticates: `TokenSet::extra` holds
        // the Cloud Code project and tier, and no token material.
        let context = minted
            .extra
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        return Ok((Some(credential), context));
    }
    let Some(reference) = &account.secret_ref else {
        return Ok((None, BTreeMap::new()));
    };
    let mut vault = state.vault.lock().await;
    let stored = match vault.get(reference) {
        Ok(Some(secret)) => Some(secret),
        Ok(None) => {
            // The conductor's vault handle is a snapshot from startup, and the panel hands it
            // accounts whose credentials were pasted in afterwards. A miss is worth one re-read
            // before it becomes an operator-facing "this account has no credential".
            if vault.reload_from_disk().is_err() {
                None
            } else {
                vault
                    .get(reference)
                    .map_err(|e| format!("vault read failed: {e}"))?
            }
        }
        Err(e) => return Err(format!("vault read failed: {e}")),
    };
    Ok((stored, BTreeMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::oauth::TokenSet;

    fn copilot_tokens() -> TokenSet {
        let mut tokens = TokenSet::new("session-token");
        tokens.refresh_token = Some("github-token".into());
        tokens
    }

    #[test]
    fn a_copilot_quota_read_uses_the_github_token_not_the_session_token() {
        // `api.github.com` accepts the GitHub OAuth token and refuses the minted session token,
        // which is the one credential in the panel that differs by destination.
        assert_eq!(
            select_token(&copilot_tokens(), "github-copilot", TokenUse::Quota),
            "github-token"
        );
    }

    #[test]
    fn a_copilot_request_uses_the_session_token() {
        assert_eq!(
            select_token(&copilot_tokens(), "github-copilot", TokenUse::Request),
            "session-token"
        );
    }

    #[test]
    fn every_other_platform_uses_its_access_token_for_both() {
        let mut tokens = TokenSet::new("access");
        tokens.refresh_token = Some("refresh".into());
        for usage in [TokenUse::Request, TokenUse::Quota] {
            assert_eq!(select_token(&tokens, "antigravity", usage), "access");
            assert_eq!(select_token(&tokens, "openai-codex", usage), "access");
        }
    }
}

//! Keeping a lane account across a conductor restart.
//!
//! A lane account is three facts and a secret: which platform, an operator label, the endpoint, and
//! a token. The facts live in the lane's in-memory registry, and until now so did the token — which
//! meant every conductor restart (every deploy, every `systemctl restart`) silently unattached every
//! lane. The operator's answer to "why is molab empty again" would be a re-attach, and a lane that
//! cannot survive the process it runs in is not a multi-account surface.
//!
//! So the token goes where the other secrets go: the encrypted vault, at `lane:<platform>:<account>`.
//! The value is the JSON object the lane needs to rebuild the registration, and nothing else — no
//! probe timestamps, no health, because those are measurements of a *running* process and a
//! restored account has not been measured yet. [`crate::lane_saturn::restore_for`] and its siblings
//! say so in the status they publish rather than inventing a healthy verdict.
//!
//! The vault is the only sanctioned place for a secret on disk. A lane token written anywhere else
//! — a log, a config field, a plaintext file — would be a regression, so [`remember`] refuses an
//! empty token rather than storing a useless entry.

use mr_core::secret::Vault;
use serde_json::Value;
use tracing::{info, warn};

use crate::state::AppState;

/// The id prefix every lane secret shares. `rental:<account>` belongs to the GPU rental surface;
/// this is deliberately a different namespace so the two can never collide.
pub const PREFIX: &str = "lane";

/// The vault id for one lane account.
///
/// The account label may contain colons: the platform is the first segment after the prefix, and
/// everything after the second colon is the label, so `lane:saturn:team:a` reads back as `team:a`.
pub fn secret_id(platform: &str, account: &str) -> String {
    format!("{PREFIX}:{platform}:{account}")
}

/// Store one lane account, replacing any earlier value for the same platform and label.
///
/// An empty token is refused: it would create an entry that looks attached and can never
/// authenticate, which is worse than having no entry at all.
pub fn remember(
    vault: &mut Vault,
    platform: &str,
    account: &str,
    value: &Value,
) -> Result<(), String> {
    if account.trim().is_empty() {
        return Err(
            "a lane account needs a label before its credential can be remembered".to_owned(),
        );
    }
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() {
        return Err("a lane credential with an empty token is not worth remembering".to_owned());
    }
    let id = secret_id(platform, account);
    vault
        .put(&id, &value.to_string())
        .map_err(|e| format!("the {platform} credential could not be stored: {e}"))?;
    vault
        .flush()
        .map_err(|e| format!("the {platform} credential could not be written to disk: {e}"))
}

/// Every remembered account for one platform, sorted by label.
///
/// A value that will not parse is skipped rather than aborting the load: one unreadable entry must
/// not cost the operator the other accounts on the same platform. The skip is reported by the
/// caller's `warn!`, not swallowed here.
pub fn remembered(vault: &Vault, platform: &str) -> Vec<(String, Value)> {
    let prefix = format!("{PREFIX}:{platform}:");
    let mut found = Vec::new();
    for id in vault.ids() {
        let Some(account) = id.strip_prefix(&prefix) else {
            continue;
        };
        if account.is_empty() {
            continue;
        }
        let Ok(Some(raw)) = vault.get(&id) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            found.push((account.to_owned(), value));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Forget one lane account. Returns whether an entry was actually removed.
pub fn forget(vault: &mut Vault, platform: &str, account: &str) -> bool {
    let removed = vault.delete(&secret_id(platform, account));
    if removed {
        let _ = vault.flush();
    }
    removed
}

/// Re-register every lane account this process can find in the vault.
///
/// Called once, before the control listener binds, so an operator who attached an account yesterday
/// still finds it today. Restoring is deliberately quiet: no probe is made here (a lane's host may
/// be a tunnel that only exists while the operator's session does), so the status routes report
/// "restored from the vault; not probed" until something actually dials the endpoint.
///
/// A malformed entry is skipped with a warning that names the platform and label but never the
/// value: one bad account must not cost the operator the others.
pub async fn restore_all(app: &AppState) {
    let (saturn, molab, cerebrium) = {
        let vault = app.vault.lock().await;
        (
            remembered(&vault, "saturn"),
            remembered(&vault, "molab"),
            remembered(&vault, "cerebrium"),
        )
    };
    let mut counts = [0usize; 3];

    for (account, value) in saturn {
        let host = field(&value, "host");
        let token = field(&value, "token");
        if host.is_empty() || token.is_empty() {
            warn!("saturn account {account} was remembered without a host or token; skipping it");
            continue;
        }
        crate::lane_saturn::restore_for(&app.saturn, account, host, token).await;
        counts[0] += 1;
    }

    for (account, value) in molab {
        let url = field(&value, "url");
        let token = field(&value, "token");
        if url.is_empty() || token.is_empty() {
            warn!("molab account {account} was remembered without a url or token; skipping it");
            continue;
        }
        crate::lane_molab::restore_for(&app.molab, account, url, token).await;
        counts[1] += 1;
    }

    for (account, value) in cerebrium {
        let project_id = field(&value, "project_id");
        let app_name = field(&value, "app_name");
        let token = field(&value, "token");
        if project_id.is_empty() || app_name.is_empty() || token.is_empty() {
            warn!(
                "cerebrium account {account} was remembered without a project, app or token; \
                 skipping it"
            );
            continue;
        }
        crate::lane_cerebrium::restore_for(&app.cerebrium, account, project_id, app_name, token)
            .await;
        counts[2] += 1;
    }

    if counts.iter().any(|n| *n > 0) {
        info!(
            "restored lane accounts from the vault: {} saturn, {} molab, {} cerebrium",
            counts[0], counts[1], counts[2]
        );
    }
}

/// One string field of a remembered entry, or the empty string when it is absent or not a string.
fn field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A vault in its own temporary directory. The name carries the process id so two test binaries
    /// running at once cannot share (and therefore corrupt) one vault.
    fn scratch_vault(tag: &str) -> (std::path::PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!(
            "mr-lane-vault-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let vault = Vault::open(&dir).expect("a fresh directory opens as an empty vault");
        (dir, vault)
    }

    #[test]
    fn an_id_keeps_the_label_whole() {
        assert_eq!(secret_id("saturn", "team-a"), "lane:saturn:team-a");
        assert_eq!(secret_id("saturn", "team:a"), "lane:saturn:team:a");
        assert_eq!(secret_id("cerebrium", "prj_1"), "lane:cerebrium:prj_1");
    }

    #[test]
    fn an_account_survives_being_written_and_read_back() {
        let (dir, mut vault) = scratch_vault("roundtrip");
        let value = json!({"host": "10.0.0.4:8000", "token": "tok"});
        remember(&mut vault, "saturn", "team-a", &value).expect("the vault accepts a value");

        // Read through a second handle, the way a restarted process would.
        let reopened = Vault::open(&dir).expect("the vault reopens");
        let found = remembered(&reopened, "saturn");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "team-a");
        assert_eq!(found[0].1, value);
        assert!(
            remembered(&reopened, "molab").is_empty(),
            "platforms do not leak"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_write_replaces_the_first() {
        let (dir, mut vault) = scratch_vault("replace");
        remember(
            &mut vault,
            "molab",
            "a",
            &json!({"url": "u1", "token": "t1"}),
        )
        .unwrap();
        remember(
            &mut vault,
            "molab",
            "a",
            &json!({"url": "u2", "token": "t2"}),
        )
        .unwrap();
        let found = remembered(&vault, "molab");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1["url"], "u2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_removes_exactly_one_account() {
        let (dir, mut vault) = scratch_vault("forget");
        remember(&mut vault, "cerebrium", "a", &json!({"token": "ta"})).unwrap();
        remember(&mut vault, "cerebrium", "b", &json!({"token": "tb"})).unwrap();
        assert!(forget(&mut vault, "cerebrium", "a"));
        assert!(
            !forget(&mut vault, "cerebrium", "a"),
            "forgetting twice is a no-op"
        );
        let left = remembered(&vault, "cerebrium");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_token_or_label_is_refused() {
        let (dir, mut vault) = scratch_vault("refuse");
        assert!(remember(
            &mut vault,
            "saturn",
            "a",
            &json!({"host": "h", "token": ""})
        )
        .is_err());
        assert!(remember(&mut vault, "saturn", "a", &json!({"host": "h"})).is_err());
        assert!(remember(&mut vault, "saturn", " ", &json!({"token": "t"})).is_err());
        assert!(remembered(&vault, "saturn").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_entry_costs_only_itself() {
        let (dir, mut vault) = scratch_vault("junk");
        remember(&mut vault, "saturn", "good", &json!({"token": "t"})).unwrap();
        vault.put(&secret_id("saturn", "bad"), "not json").unwrap();
        vault.flush().unwrap();
        let found = remembered(&vault, "saturn");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

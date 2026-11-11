//! Thought signatures, carried across the client round trip.
//!
//! Gemini 3 and later attach an opaque `thoughtSignature` to the `functionCall` parts of their
//! answer, and demand that same signature back on every later turn in which that call is replayed.
//! A hub-and-spoke router breaks this by construction: the Chat Completions pivot has nowhere to
//! put a signature, so an OpenAI-shaped client echoes only the call id and the signature is lost.
//! Google answers that loss with a 400 — "Function call is missing a thought_signature in
//! functionCall parts" — which fails the whole turn, not just the tool call.
//!
//! The fix has two halves, both in this module:
//!
//! * Every function call we hand a client is given a **globally unique id** and the signature that
//!   came with it is kept against that id, in memory, for the lifetime of the process. When the
//!   client echoes the id back, the real signature rides out again and the model keeps its
//!   continuity across the tool call.
//! * When the id is unknown — a replay after a restart, a client that rewrites ids, an Anthropic
//!   `toolu_…` this router never minted — the part carries Google's documented stand-in,
//!   [`UNKNOWN_SIGNATURE`], which tells the model the original is unavailable instead of failing
//!   the request outright.
//!
//! The store is memory-only by design. Signatures are tied to one model's one turn; persisting
//! them would outlive their meaning and make a stale signature — which Google rejects as a
//! mismatch, worse than no signature — reachable from a later process.
//!
//! Uniqueness matters more than it looks: ids are what the client echoes, so two answers sharing
//! an id would make one conversation replay another's signature. A process-local counter alone
//! restarts at zero, so the first mint also draws a salt from the clock and the counter always
//! runs forward from there.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// What Google accepts in place of a signature that is genuinely not available.
///
/// Its documentation names this literal as the opt-out from signature validation, for callers that
/// legitimately never received one. It is always better than an empty part: the request succeeds,
/// and the model simply cannot lean on the original reasoning.
pub(crate) const UNKNOWN_SIGNATURE: &str = "skip_thought_signature_validator";

/// How many signatures are kept. A conversation replays only its own recent calls, so this bound
/// covers many concurrent conversations without letting a long-running process grow without limit.
const CAPACITY: usize = 4096;

struct Store {
    by_id: HashMap<String, String>,
    order: VecDeque<String>,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);
static SALT: OnceLock<u64> = OnceLock::new();

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| {
        Mutex::new(Store {
            by_id: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

fn salt() -> u64 {
    *SALT.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    })
}

/// A function-call id that is unique across processes, so a signature can be resolved by id alone.
pub(crate) fn mint_call_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{:x}{:04x}", salt(), n & 0xffff_ffff)
}

/// Keep the signature that came with `id` for the turn that will replay it.
pub(crate) fn remember(id: &str, signature: &str) {
    if id.is_empty() || signature.is_empty() {
        return;
    }
    let Ok(mut store) = store().lock() else {
        // A poisoned lock means some other thread panicked while holding it. Losing a signature
        // downgrades the next replay to the stand-in; failing the request would be worse.
        return;
    };
    if store
        .by_id
        .insert(id.to_string(), signature.to_string())
        .is_none()
    {
        store.order.push_back(id.to_string());
    }
    while store.by_id.len() > CAPACITY {
        match store.order.pop_front() {
            Some(old) => {
                store.by_id.remove(&old);
            }
            None => break,
        }
    }
}

/// The signature recorded for `id`, if this process minted it and still remembers.
pub(crate) fn signature(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let store = store().lock().ok()?;
    store.by_id.get(id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_id_resolves_its_own_signature() {
        let id = mint_call_id();
        remember(&id, "sig-abc");
        assert_eq!(signature(&id).as_deref(), Some("sig-abc"));
    }

    #[test]
    fn ids_never_repeat_within_a_process() {
        let a = mint_call_id();
        let b = mint_call_id();
        assert_ne!(a, b);
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        assert_eq!(signature("call_never_minted_by_this_process"), None);
        assert_eq!(signature(""), None);
    }

    #[test]
    fn the_oldest_signature_is_dropped_past_capacity() {
        let first = mint_call_id();
        remember(&first, "old");
        for _ in 0..CAPACITY {
            let id = mint_call_id();
            remember(&id, "filler");
        }
        assert_eq!(
            signature(&first),
            None,
            "the oldest entry should be evicted"
        );
    }
}

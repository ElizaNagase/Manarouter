//! Reasoning effort reconciliation for upstream providers.
//!
//! When an upstream returns a 4xx client error (typically 400 Bad Request or 422 Unprocessable)
//! rejecting a `reasoning_effort` parameter, this module extracts the acceptable values from the
//! upstream error message, finds the closest match by ordinal rank, or strips the parameter if
//! unsupported.

use serde_json::Value;

/// Standard ordinal ranks for reasoning efforts.
/// Higher index = higher effort.
const KNOWN_RANKS: &[(&str, i32)] = &[
    ("none", 0),
    ("minimal", 1),
    ("low", 2),
    ("medium", 3),
    ("high", 4),
    ("xhigh", 5),
    ("max", 6),
];

/// Get the ordinal rank for a reasoning effort name, comparing case-insensitively.
pub fn effort_rank(effort: &str) -> Option<i32> {
    let lower = effort.trim().to_ascii_lowercase();
    for &(name, rank) in KNOWN_RANKS {
        if lower == name {
            return Some(rank);
        }
    }
    None
}

/// Action to take on the outbound request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Remap `reasoning_effort` to the nearest supported value.
    Remap(String),
    /// Drop `reasoning_effort` from the request body altogether.
    DropField,
}

/// Inspect an upstream error body and the current requested effort.
///
/// If the error indicates an unsupported `reasoning_effort`, returns the action to reconcile it.
pub fn inspect_refusal(
    error_body: &str,
    requested_effort: Option<&str>,
) -> Option<ReconcileAction> {
    let lower = error_body.to_ascii_lowercase();

    // Check if the refusal relates to reasoning effort / thinking budget
    let mentions_reasoning = lower.contains("reasoning_effort")
        || lower.contains("reasoning effort")
        || lower.contains("reasoning-effort")
        || lower.contains("thinking_budget")
        || lower.contains("unsupported_reasoning_effort");

    if !mentions_reasoning {
        return None;
    }

    // Try to extract an enumerated list of acceptable values
    let candidates = extract_candidate_values(&lower);
    if !candidates.is_empty() {
        if let Some(req) = requested_effort {
            if let Some(closest) = find_closest_effort(req, &candidates) {
                return Some(ReconcileAction::Remap(closest));
            }
        }
        // If we found candidates but couldn't map or had no requested effort, drop the field
        return Some(ReconcileAction::DropField);
    }

    // Mentions reasoning_effort but gives no usable list of choices:
    // Drop the field (honest degradation)
    Some(ReconcileAction::DropField)
}

/// Extract candidate values from error message text.
fn extract_candidate_values(text: &str) -> Vec<String> {
    // Look for phrases like "must be ...", "supported values ...", "valid values ...", "one of: ..."
    let marker_patterns = [
        "must be",
        "supported values are",
        "supported values:",
        "supported are",
        "valid values are",
        "valid values:",
        "one of:",
        "one of",
        "allowed values are",
        "allowed values:",
        "choose from:",
        "choose from",
    ];

    let mut search_slice = text;
    for marker in marker_patterns {
        if let Some(idx) = search_slice.find(marker) {
            search_slice = &search_slice[idx + marker.len()..];
            break;
        }
    }

    // Collect words from the slice that look like effort names
    let mut results = Vec::new();
    let cleaned = search_slice.replace(
        ['[', ']', '(', ')', '{', '}', '\'', '"', '`', '.', ':', ';'],
        " ",
    );

    for raw in cleaned.split([',', ' ', '\n', '\t']) {
        let token = raw.trim();
        if token.is_empty() || token == "or" || token == "and" {
            continue;
        }
        // If the token matches a known effort or is a reasonable identifier
        if effort_rank(token).is_some()
            && !results
                .iter()
                .any(|r: &String| r.eq_ignore_ascii_case(token))
        {
            results.push(token.to_string());
        }
    }

    results
}

/// Find the closest supported effort to the requested one by ordinal rank.
///
/// On a tie between two equidistant values, picks the HIGHER rank.
pub fn find_closest_effort(requested: &str, candidates: &[String]) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }

    let req_rank = effort_rank(requested).unwrap_or(3); // Default to medium rank if unknown

    let mut best_match: Option<&String> = None;
    let mut min_distance = i32::MAX;
    let mut best_rank = i32::MIN;

    for cand in candidates {
        let rank = match effort_rank(cand) {
            Some(r) => r,
            None => continue,
        };

        let distance = (req_rank - rank).abs();
        if distance < min_distance {
            min_distance = distance;
            best_match = Some(cand);
            best_rank = rank;
        } else if distance == min_distance {
            // Tie-breaking: pick the HIGHER rank
            if rank > best_rank {
                min_distance = distance;
                best_match = Some(cand);
                best_rank = rank;
            }
        }
    }

    best_match.cloned()
}

/// Apply a reconciliation action to an outgoing JSON request body.
///
/// Returns `Some(new_bytes)` if modified, or `None` if no change was made.
pub fn apply_reconcile_to_body(body_bytes: &[u8], action: &ReconcileAction) -> Option<Vec<u8>> {
    let mut val: Value = serde_json::from_slice(body_bytes).ok()?;
    let obj = val.as_object_mut()?;

    match action {
        ReconcileAction::Remap(new_val) => {
            obj.insert(
                "reasoning_effort".to_string(),
                Value::String(new_val.clone()),
            );
        }
        ReconcileAction::DropField => {
            let removed = obj.remove("reasoning_effort");
            if removed.is_none() {
                // Also check thinking / thinking_config if any
                obj.remove("thinking");
            }
        }
    }

    serde_json::to_vec(&val).ok()
}

/// Read the current `reasoning_effort` from an outgoing JSON body, if present.
pub fn extract_requested_effort(body_bytes: &[u8]) -> Option<String> {
    let val: Value = serde_json::from_slice(body_bytes).ok()?;
    val.get("reasoning_effort")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_closest_effort_exact_match() {
        let candidates = vec!["low".to_string(), "medium".to_string(), "xhigh".to_string()];
        assert_eq!(
            find_closest_effort("low", &candidates),
            Some("low".to_string())
        );
        assert_eq!(
            find_closest_effort("medium", &candidates),
            Some("medium".to_string())
        );
        assert_eq!(
            find_closest_effort("xhigh", &candidates),
            Some("xhigh".to_string())
        );
    }

    #[test]
    fn test_find_closest_effort_tie_breaker_picks_higher() {
        // "high" has rank 4.
        // candidates: "medium" (rank 3, diff 1), "xhigh" (rank 5, diff 1).
        // Equidistant! Tie breaker must pick the higher rank: "xhigh".
        let candidates = vec!["low".to_string(), "medium".to_string(), "xhigh".to_string()];
        assert_eq!(
            find_closest_effort("high", &candidates),
            Some("xhigh".to_string())
        );

        // "minimal" has rank 1.
        // candidates: "none" (rank 0, diff 1), "low" (rank 2, diff 1).
        // Equidistant! Tie breaker must pick "low".
        let candidates2 = vec!["none".to_string(), "low".to_string()];
        assert_eq!(
            find_closest_effort("minimal", &candidates2),
            Some("low".to_string())
        );
    }

    #[test]
    fn test_find_closest_effort_case_insensitive() {
        let candidates = vec!["LOW".to_string(), "Medium".to_string(), "XHIGH".to_string()];
        assert_eq!(
            find_closest_effort("HIGH", &candidates),
            Some("XHIGH".to_string())
        );
        assert_eq!(
            find_closest_effort("low", &candidates),
            Some("LOW".to_string())
        );
    }

    #[test]
    fn test_inspect_refusal_iteracompute() {
        let err = r#"{"error":{"message":"reasoning effort must be low, medium, xhigh","type":"invalid_request_error","code":"unsupported_reasoning_effort"}}"#;
        let action = inspect_refusal(err, Some("high"));
        assert_eq!(action, Some(ReconcileAction::Remap("xhigh".to_string())));
    }

    #[test]
    fn test_inspect_refusal_openai_style() {
        let err = r#"{"error":{"message":"Invalid value for 'reasoning_effort': 'extreme'. Supported values are: 'low', 'medium', 'high'."}}"#;
        let action = inspect_refusal(err, Some("extreme"));
        // "extreme" is unknown, defaults to rank 3 ("medium"). Closest is "medium".
        assert!(matches!(action, Some(ReconcileAction::Remap(_))));
    }

    #[test]
    fn test_inspect_refusal_no_candidates_drops_field() {
        let err = r#"{"error":{"message":"The model does not support reasoning_effort parameter.","type":"invalid_request_error"}}"#;
        let action = inspect_refusal(err, Some("high"));
        assert_eq!(action, Some(ReconcileAction::DropField));
    }

    #[test]
    fn test_inspect_refusal_unrelated_error_ignored() {
        let err = r#"{"error":{"message":"The model is overloaded.","type":"rate_limit_error"}}"#;
        assert_eq!(inspect_refusal(err, Some("high")), None);
    }

    #[test]
    fn test_apply_reconcile_to_body() {
        let original = br#"{"model":"qwen","messages":[],"reasoning_effort":"high"}"#;
        let remap_act = ReconcileAction::Remap("xhigh".to_string());
        let res = apply_reconcile_to_body(original, &remap_act).unwrap();
        let val: Value = serde_json::from_slice(&res).unwrap();
        assert_eq!(val["reasoning_effort"], "xhigh");

        let drop_act = ReconcileAction::DropField;
        let res_drop = apply_reconcile_to_body(original, &drop_act).unwrap();
        let val_drop: Value = serde_json::from_slice(&res_drop).unwrap();
        assert!(val_drop.get("reasoning_effort").is_none());
    }
}

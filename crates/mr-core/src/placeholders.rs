//! Named placeholders in a catalogue base URL.
//!
//! Two shipped providers cannot spell their endpoint without a value the operator owns: Cloudflare
//! Workers AI addresses an account (`…/client/v4/accounts/{account_id}/…`) and Snowflake addresses
//! a whole hostname (`https://{account}.snowflakecomputing.com/api/v2`). The value belongs to the
//! *account*, not the provider — one provider, several accounts, several values — so a catalogue
//! entry carries a name and the account carries the value.
//!
//! This module is the substitution rule itself. It is deliberately free of provider knowledge: the
//! name `{account_id}` is not special, and a provider that invents `{workspace}` tomorrow needs no
//! code change. What it does know is what a *value* may look like, because the substitution is a
//! splice into a URL: an account id is a path component and a Snowflake account name is part of a
//! hostname, and neither may carry `/`, `?`, `#`, `@` or whitespace. Refusing those by name is the
//! difference between a typo and a request smuggled to another host.
//!
//! An unfilled name is reported as a list, never rendered back into the URL: sending `{account_id}`
//! to the upstream produces a 404 that blames the provider for a local configuration mistake.

/// Why a template could not be filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The template names placeholders the account does not supply.
    Missing {
        /// Every name that had no value, in the order it appears in the template, deduplicated.
        names: Vec<String>,
    },
    /// A value was supplied but cannot be spliced into a URL safely.
    UnusableValue {
        /// The placeholder whose value was refused.
        name: String,
        /// Why the value was refused, phrased for an operator.
        reason: &'static str,
    },
}

impl Problem {
    /// A message naming what the operator has to do.
    pub fn message(&self) -> String {
        match self {
            Self::Missing { names } => {
                let names = names
                    .iter()
                    .map(|name| format!("{{{name}}}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "the endpoint needs {names}, which this account does not supply; set the \
                     account's placeholders to a URL-safe value"
                )
            }
            Self::UnusableValue { name, reason } => {
                format!("the value for {{{name}}} is unusable: {reason}")
            }
        }
    }
}

/// The longest placeholder name accepted.
///
/// A name is `{` + up to 64 characters + `}`, which is far longer than any provider needs and
/// short enough that a stray brace in a path is not mistaken for a placeholder.
const MAX_NAME_LEN: usize = 64;

/// Whether `byte` may appear in a placeholder name.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether a placeholder name is well formed.
///
/// A name starts with a letter or an underscore and continues with letters, digits or underscores.
/// Requiring that shape is what keeps `{ "model": "x" }` — JSON that happens to be in a string —
/// from being read as a placeholder.
fn is_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(is_name_byte)
}

/// Whether a substituted value may be spliced into a URL.
///
/// The unreserved set of RFC 3986 §2.3 plus the two separators a hostname and a path component
/// genuinely use (`-`, `.`, `_`, `~` are already unreserved; the rest is refused). Percent-encoding
/// is refused too: a value that is already encoded would be encoded again by the HTTP client.
fn is_url_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

/// Every placeholder name `template` mentions, in order, without duplicates.
pub fn names(template: &str) -> Vec<String> {
    let bytes = template.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && end - start < MAX_NAME_LEN && is_name_byte(bytes[end]) {
            end += 1;
        }
        if end > start && end < bytes.len() && bytes[end] == b'}' {
            let name = &template[start..end];
            if is_name(name) && !found.iter().any(|seen| seen == name) {
                found.push(name.to_owned());
            }
            index = end + 1;
        } else {
            index += 1;
        }
    }
    found
}

/// Fill every `{name}` in `template` from `values`.
///
/// # Errors
///
/// [`Problem::Missing`] when the template mentions a name the map does not hold, and
/// [`Problem::UnusableValue`] when a value is not safe to splice into a URL. Both are returned
/// rather than silently leaving the braces in place, because a literal `{account_id}` reaching an
/// upstream produces a 404 that blames the wrong party.
pub fn substitute<S: std::borrow::Borrow<str>>(
    template: &str,
    values: &std::collections::BTreeMap<String, S>,
) -> Result<String, Problem> {
    let names = names(template);
    if names.is_empty() {
        return Ok(template.to_owned());
    }
    let missing: Vec<String> = names
        .iter()
        .filter(|name| !values.contains_key(name.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(Problem::Missing { names: missing });
    }
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && end - start < MAX_NAME_LEN && is_name_byte(bytes[end]) {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b'}' {
                let name = &template[start..end];
                if is_name(name) {
                    let value: &str = values[name].borrow();
                    if !is_url_safe(value) {
                        return Err(Problem::UnusableValue {
                            name: name.to_owned(),
                            reason: "a URL component may only contain letters, digits, '-', '.', \
                                     '_' and '~'",
                        });
                    }
                    out.push_str(value);
                    index = end + 1;
                    continue;
                }
            }
        }
        // Not a placeholder: copy the byte and move on. Multi-byte UTF-8 is copied byte by byte,
        // which preserves the string because only ASCII bytes are ever interpreted.
        out.push(bytes[index] as char);
        index += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn a_template_without_placeholders_is_returned_unchanged() {
        let template = "https://api.openai.com/v1/chat/completions";
        assert_eq!(substitute(template, &values(&[])), Ok(template.to_owned()));
    }

    #[test]
    fn cloudflares_account_id_is_filled_from_the_account() {
        let template =
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1/chat/completions";
        let filled = substitute(template, &values(&[("account_id", "0123456789abcdef")]));
        assert_eq!(
            filled,
            Ok(
                "https://api.cloudflare.com/client/v4/accounts/0123456789abcdef/ai/v1/chat/completions"
                    .to_owned()
            )
        );
    }

    #[test]
    fn snowflakes_hostname_component_is_the_same_rule() {
        // A second provider whose placeholder is part of the *host*, not the path: the rule is
        // named placeholders, not "an account id field", which is why this needs no second code
        // path.
        let template = "https://{account}.snowflakecomputing.com/api/v2";
        assert_eq!(
            substitute(template, &values(&[("account", "org-abc123")])),
            Ok("https://org-abc123.snowflakecomputing.com/api/v2".to_owned())
        );
    }

    #[test]
    fn a_missing_name_is_reported_and_never_left_in_the_url() {
        let problem = substitute(
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai",
            &values(&[]),
        )
        .expect_err("an unfilled placeholder must not be returned");
        assert_eq!(
            problem,
            Problem::Missing {
                names: vec!["account_id".to_owned()]
            }
        );
        assert!(problem.message().contains("{account_id}"));
    }

    #[test]
    fn every_missing_name_is_reported_once_in_template_order() {
        let problem = substitute(
            "https://{account}.example.com/{account}/{workspace}",
            &values(&[]),
        )
        .expect_err("nothing was supplied");
        assert_eq!(
            problem,
            Problem::Missing {
                names: vec!["account".to_owned(), "workspace".to_owned()]
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_url_component_is_refused() {
        // A value with a slash would move the request to another path, and one with `@` could move
        // it to another host. Both are refused rather than encoded.
        for hostile in ["../etc", "a/b", "user@evil.example", "a b", "a?b", ""] {
            let problem = substitute(
                "https://x.example/{account}",
                &values(&[("account", hostile)]),
            )
            .expect_err("a URL-unsafe value must be refused");
            match problem {
                Problem::UnusableValue { name, .. } => assert_eq!(name, "account"),
                other => panic!("expected UnusableValue for {hostile:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn braces_that_are_not_placeholder_shaped_are_left_alone() {
        // A route override could legitimately carry JSON or a run of braces; only `{name}` is a
        // placeholder, so nothing else is touched and nothing is reported missing.
        let template = "https://x.example/v1/{}%7B%7D/{not a name}/{account}";
        assert_eq!(
            names(template),
            vec!["account".to_owned()],
            "only well-formed names count"
        );
        assert_eq!(
            substitute(template, &values(&[("account", "work")])),
            Ok("https://x.example/v1/{}%7B%7D/{not a name}/work".to_owned())
        );
    }

    #[test]
    fn an_unterminated_brace_is_not_a_placeholder() {
        assert!(names("https://x.example/{account").is_empty());
        assert!(names("https://x.example/{").is_empty());
        assert_eq!(
            substitute("https://x.example/{account", &values(&[])),
            Ok("https://x.example/{account".to_owned())
        );
    }

    #[test]
    fn a_name_longer_than_the_limit_is_not_a_placeholder() {
        let long = "a".repeat(65);
        assert!(names(&format!("https://x.example/{{{long}}}")).is_empty());
    }

    #[test]
    fn a_name_starting_with_a_digit_is_not_a_placeholder() {
        assert!(names("https://x.example/{0account}").is_empty());
        assert_eq!(
            names("https://x.example/{_account}"),
            vec!["_account".to_owned()]
        );
    }
}

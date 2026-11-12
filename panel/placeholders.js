// placeholders.js — the rules for a named placeholder in a provider base URL.
//
// A base URL may name a value the *account* owns rather than the provider: `cloudflare-ai`
// addresses `…/client/v4/accounts/{account_id}/…` and Snowflake puts `{account}` in the hostname.
// The relay substitutes per account (`mr-core/src/placeholders.rs`), so an operator needs a place to
// type that value. This module holds the two rules the editor and a test share:
//
//   * which `{name}` tokens a template mentions, by the same scan the relay uses, so the panel
//     offers exactly the names a request would fail on — a stray brace in a path is not a
//     placeholder and a name with a dash in it is not one either;
//   * what a name and a value may look like. The value rule is the narrow one: an account id is a
//     path component and a Snowflake account name is part of a hostname, so `-`, `.`, `_` and `~`
//     are the only punctuation that cannot change which host or which path a request reaches. The
//     server refuses anything else rather than percent-encoding it, because a value that is
//     already encoded would be encoded again.

/// A placeholder name: a letter or underscore, then letters, digits or underscores, at most 64.
export const NAME_PATTERN = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

/// A value that may be spliced into a URL: RFC 3986 unreserved characters, never empty.
export const VALUE_PATTERN = /^[A-Za-z0-9._~-]+$/;

/// Every placeholder name `template` mentions, in order, without duplicates.
export function placeholderNamesIn(template) {
  const names = [];
  for (const match of String(template ?? '').matchAll(/\{([A-Za-z0-9_]{1,64})\}/g)) {
    const name = match[1];
    if (!NAME_PATTERN.test(name) || names.includes(name)) continue;
    names.push(name);
  }
  return names;
}

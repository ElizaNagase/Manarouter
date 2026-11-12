// The rules a named placeholder obeys, pinned against the relay's own scrape
// (`mr-core/src/placeholders.rs`): the panel must offer exactly the names a request would fail on,
// and must refuse exactly the values the server refuses rather than encoding them.
//
// Run: node --test panel/test/placeholders.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

const { placeholderNamesIn, NAME_PATTERN, VALUE_PATTERN } = await import('../placeholders.js');

test('the two shipped templated base URLs yield the names they need', () => {
  assert.deepEqual(
    placeholderNamesIn('https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1'),
    ['account_id'],
  );
  assert.deepEqual(
    placeholderNamesIn('https://{account}.snowflakecomputing.com/api/v2'),
    ['account'],
  );
});

test('an ordinary URL mentions no placeholder', () => {
  assert.deepEqual(placeholderNamesIn('https://api.openai.com/v1'), []);
  assert.deepEqual(placeholderNamesIn(''), []);
  assert.deepEqual(placeholderNamesIn(null), []);
});

test('a brace that is not a placeholder is not read as one', () => {
  // JSON in a path, an empty pair, an unclosed brace, a dotted or dashed name: the relay skips all
  // of these, so offering them for editing would invent a name nothing substitutes.
  assert.deepEqual(placeholderNamesIn('https://x.test/{ "model": "x" }'), []);
  assert.deepEqual(placeholderNamesIn('https://x.test/{}'), []);
  assert.deepEqual(placeholderNamesIn('https://x.test/{account'), []);
  assert.deepEqual(placeholderNamesIn('https://x.test/{account-id}'), []);
  assert.deepEqual(placeholderNamesIn('https://x.test/{9lives}'), []);
});

test('names come back in template order and once each', () => {
  assert.deepEqual(
    placeholderNamesIn('https://{account}.x.test/{account_id}/{account}/{account_id}'),
    ['account', 'account_id'],
  );
});

test('the name rule is a letter or underscore then word characters, at most 64', () => {
  assert.ok(NAME_PATTERN.test('account_id'));
  assert.ok(NAME_PATTERN.test('_x'));
  assert.ok(NAME_PATTERN.test('a'.repeat(64)));
  assert.ok(!NAME_PATTERN.test('a'.repeat(65)));
  assert.ok(!NAME_PATTERN.test('9lives'));
  assert.ok(!NAME_PATTERN.test('account-id'));
  assert.ok(!NAME_PATTERN.test(''));
});

test('the value rule is the unreserved set and never empty', () => {
  for (const value of ['abc', 'A1', '0f5e-1', 'a.b', 'a_b', 'a~b']) {
    assert.ok(VALUE_PATTERN.test(value), `${value} should be usable`);
  }
  for (const value of ['', 'a/b', 'a b', 'a?b', 'a#b', 'a@b', 'a%20b', 'caf\u00e9']) {
    assert.ok(!VALUE_PATTERN.test(value), `${value} should be refused`);
  }
});

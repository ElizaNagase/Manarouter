// The panel's half of quota probing: the candidate list it edits, and how it reads one back.
//
// The router walks a list of conventional balance/quota paths against an account's own base URL
// when the provider has no dedicated fetcher (`crates/mr-core/src/quota/probe.rs`). Settings edits
// that list and the Quota page says which candidate answered, so two things here are worth pinning:
// the built-in list the card shows must be the one the router would walk, and a path the router
// would refuse must be refused while it is being typed, not at the first refresh.
//
// Not covered here: the card's DOM (rows, reorder buttons, the save round trip). That needs a
// browser — open Settings, reorder a candidate, save, then refresh an account on the Quota page and
// confirm the provenance line names the candidate the router actually read.
//
// Run: node --test panel/test/quota-probe.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  probePathListProblem,
  probePathProblem,
  probeSourceLabel,
  QUOTA_PROBE_CHAT_PATH,
  QUOTA_PROBE_DEFAULT_PATHS,
} from '../format.js';

test('the built-in candidate list is the one the router walks, in order', () => {
  // Pinned literally, not read from the module's own table: the point is to catch this copy
  // drifting from DEFAULT_QUOTA_PROBE_PATHS in crates/mr-core/src/quota/probe.rs, and comparing a
  // constant to itself would never do that.
  assert.deepEqual(QUOTA_PROBE_DEFAULT_PATHS, [
    '/v1/balance',
    '/api/balance',
    '/api/v1/key',
    '/v1/key',
    '/v1/user/info',
    '/v1/dashboard/billing/subscription',
    '/v1/credits',
    '/v1/quota',
    '/v1/usage',
  ]);
  assert.equal(QUOTA_PROBE_CHAT_PATH, '/v1/chat/completions');
});

test('every built-in candidate is one the router would accept', () => {
  for (const path of QUOTA_PROBE_DEFAULT_PATHS) {
    assert.equal(probePathProblem(path), null, `${path} should be usable as a candidate`);
  }
  assert.equal(probePathProblem(QUOTA_PROBE_CHAT_PATH), null);
});

test('a candidate must be a path on the account\'s own base URL', () => {
  // Absolute URLs and protocol-relative spellings are refused rather than fetched: the account's
  // credential may not be sent to another host.
  for (const bad of ['https://api.example.com/v1/balance', 'http://127.0.0.1:8080/v1/balance', '//api.example.com/v1/balance']) {
    assert.match(probePathProblem(bad) ?? '', /names a host/, bad);
  }
  // A path that does not start with a slash would be joined to the base URL incorrectly.
  assert.match(probePathProblem('v1/balance') ?? '', /must start with/);
  // A fragment is never sent to a server, so it would silently change what the router asks for.
  assert.match(probePathProblem('/v1/balance#frag') ?? '', /fragment/);
  // Empty entries are the residue of pressing "Add candidate" and not typing anything.
  assert.match(probePathProblem('') ?? '', /is empty/);
  assert.match(probePathProblem('   ') ?? '', /is empty/);
  assert.match(probePathProblem(null) ?? '', /is empty/);
});

test('a space or control character in a candidate is refused, as the router refuses it', () => {
  // Exactly the set the router rejects (probe_path_problem): a space, or \u0000-\u001f and \u007f.
  assert.match(probePathProblem('/v1/ba lance') ?? '', /space or control/);
  assert.match(probePathProblem('/v1/ba\u0000l') ?? '', /space or control/);
  assert.match(probePathProblem('/v1/ba\u007fl') ?? '', /space or control/);
  // A tab is a control character, and a trailing newline is how a pasted value usually arrives.
  assert.match(probePathProblem('/v1/balance\t') ?? '', /space or control/);
  assert.match(probePathProblem('/v1/balance\n') ?? '', /space or control/);
  // An ordinary path is untouched.
  assert.equal(probePathProblem('/v1/balance?scope=all'), null);
});

test('the first unusable candidate is the one reported, by index', () => {
  assert.equal(probePathListProblem(['/v1/balance', '/v1/credits']), null);
  // An empty list is a decision, not a malformed list: it means "probe nothing here".
  assert.equal(probePathListProblem([]), null);
  assert.equal(probePathListProblem(null), null);
  assert.deepEqual(probePathListProblem(['/v1/balance', '', '/v1/credits']), { index: 1, problem: 'is empty' });
  // Only the first problem: the card edits one row at a time, and every problem at once would put
  // a wall of text under a list that is already visibly marked.
  assert.deepEqual(
    probePathListProblem(['https://a.example.com/x', 'v1/y']),
    { index: 0, problem: probePathProblem('https://a.example.com/x') },
  );
});

test('a reading names the candidate that answered, and the part of the reply it came from', () => {
  assert.equal(probeSourceLabel('openai-compatible /v1/balance'), 'read from /v1/balance');
  assert.equal(probeSourceLabel('openai-compatible /v1/balance (headers)'), 'read from /v1/balance (response headers)');
  // The walk trims the stamp, but a hand-written config snapshot may not; neither should print
  // "read from undefined".
  assert.equal(probeSourceLabel('  openai-compatible /api/v1/key  '), 'read from /api/v1/key');
});

test('a reading from anything other than the walk says nothing about a candidate', () => {
  // A dedicated fetcher names itself, and a snapshot taken before the walk existed has no source
  // at all; in both cases the page prints only the age.
  assert.equal(probeSourceLabel(null), '');
  assert.equal(probeSourceLabel(undefined), '');
  assert.equal(probeSourceLabel(''), '');
  assert.equal(probeSourceLabel('anthropic'), '');
  assert.equal(probeSourceLabel('openai-compatible'), '');
  assert.equal(probeSourceLabel('openai-compatible-v2 /v1/balance'), '');
  // A source that lost its leading slash is not a candidate this page can name.
  assert.equal(probeSourceLabel('openai-compatible v1/balance'), '');
  assert.equal(probeSourceLabel(42), '');
});
// The panel reads the control plane over fetch(), and every read has to be a real read.
//
// A browser is free to reuse a cached GET response when the origin says nothing about caching,
// and the conductor used to say nothing. That matters here more than in an ordinary app: the
// health poll watches the routing-snapshot digest for changes, so a cached /api/health body looks
// exactly like a router nobody has touched, and a cached /api/providers leaves the tab showing a
// provider list that predates the write. cache: 'no-store' is the client-side half of that fix;
// serve.rs sends the header for the static assets.
import assert from 'node:assert/strict';
import test from 'node:test';

const calls = [];
globalThis.fetch = async (path, options) => {
  calls.push({ path, options });
  return { ok: true, status: 200, json: async () => ({ ok: true }) };
};

const { request } = await import('../api.js');

test('a control-plane read never accepts a cached body', async () => {
  const payload = await request('GET', '/api/health');
  assert.deepEqual(payload, { ok: true });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].path, '/api/health');
  assert.equal(calls[0].options.cache, 'no-store');
  assert.equal(calls[0].options.credentials, 'same-origin');
});

test('a write still serialises its body', async () => {
  await request('POST', '/api/providers', { body: { id: 'x' } });
  const last = calls.at(-1);
  assert.equal(last.options.method, 'POST');
  assert.equal(last.options.body, '{"id":"x"}');
  assert.equal(last.options.headers['content-type'], 'application/json');
});

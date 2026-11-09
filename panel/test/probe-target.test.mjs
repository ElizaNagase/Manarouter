// A live-health check sends one completion per step, which only makes sense for a step that
// names one model. A combo-target step expands to another combo's steps at compile time, so the
// panel must not post /api/models/test with an empty provider and model for it.
//
// Run: node --test panel/test/probe-target.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { probeTarget } from '../format.js';

test('a model step yields the pair the probe calls', () => {
  assert.deepEqual(
    probeTarget({ provider: 'deepseek', model: 'deepseek-chat' }),
    { provider: 'deepseek', model: 'deepseek-chat' },
  );
});

test('a combo-target step yields no probe, even if a stale provider/model is present', () => {
  assert.equal(probeTarget({ combo: 'cheap' }), null);
  assert.equal(probeTarget({ combo: 'cheap', provider: 'deepseek', model: 'deepseek-chat' }), null);
});

test('a step with no single provider/model pair is not probed', () => {
  assert.equal(probeTarget({ provider: '', model: 'm' }), null);
  assert.equal(probeTarget({ provider: 'p', model: '  ' }), null);
  assert.equal(probeTarget(null), null);
});

test('the returned pair is trimmed, so a whitespace-only field never reaches the API', () => {
  assert.deepEqual(
    probeTarget({ provider: '  deepseek  ', model: ' deepseek-chat ' }),
    { provider: 'deepseek', model: 'deepseek-chat' },
  );
});

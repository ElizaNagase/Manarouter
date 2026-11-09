// Shape checks for the quota colour policy in panel/format.js.
//
// The quota page reads its colours from one place: quotaStatusLevel() picks a tone from what is
// REMAINING, and every bar, chip and card border consumes that tone. These checks pin the
// boundaries, because an off-by-one here silently paints a nearly-exhausted window green.
//
// Not covered here: the rendered DOM (chip strip, bars, reset lines). That needs a browser —
// open the Quota page and confirm the chips jump to their card, the bars fill from the left, and
// the colour matches the percentage written beside the bar.
//
// Run: node --test panel/test/quota-display.test.mjs

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  QUOTA_STATUS_CRITICAL_AT_OR_BELOW,
  QUOTA_STATUS_HEALTHY_ABOVE,
  chunkFill,
  formatQuotaUsage,
  quotaStatusLevel,
  worstQuotaStatus,
} from '../format.js';

test('the healthy band is open above 50% and the degraded band closes at 20%', () => {
  assert.equal(QUOTA_STATUS_HEALTHY_ABOVE, 50);
  assert.equal(QUOTA_STATUS_CRITICAL_AT_OR_BELOW, 20);

  assert.equal(quotaStatusLevel(100), 'ok');
  assert.equal(quotaStatusLevel(50.1), 'ok');
  assert.equal(quotaStatusLevel(50), 'alert');
  assert.equal(quotaStatusLevel(20.1), 'alert');
  assert.equal(quotaStatusLevel(20), 'critical');
  assert.equal(quotaStatusLevel(0), 'critical');
});

test('a provider that reported no fraction is unknown, never critical', () => {
  // Red would claim the window is exhausted; the truth is that nothing was measured.
  assert.equal(quotaStatusLevel(null), 'unknown');
  assert.equal(quotaStatusLevel(undefined), 'unknown');
  assert.equal(quotaStatusLevel(NaN), 'unknown');
  assert.equal(quotaStatusLevel('not a number'), 'unknown');
});

test('the worst window decides the account, and an empty list stays unknown', () => {
  assert.equal(worstQuotaStatus([]), 'unknown');
  assert.equal(worstQuotaStatus(['unknown']), 'unknown');
  assert.equal(worstQuotaStatus(['ok']), 'ok');
  assert.equal(worstQuotaStatus(['ok', 'unknown']), 'ok');
  assert.equal(worstQuotaStatus(['ok', 'alert']), 'alert');
  assert.equal(worstQuotaStatus(['alert', 'critical']), 'critical');
  assert.equal(worstQuotaStatus(['critical', 'ok']), 'critical');
});

test('a window with a reported fraction becomes a percentage the view can colour', () => {
  // total > 0 is what tells the helper the percentage is a real measurement, not a sentinel.
  const usage = formatQuotaUsage(
    { fraction_reported: true, remaining_percentage: 73.2, total: 1000 }, { mode: 'percentage' });
  assert.equal(usage.kind, 'percentage');
  assert.equal(usage.remainingPercent, 73.2);
  assert.ok(Math.abs(usage.usedPercent - 26.8) < 1e-9);
  assert.equal(quotaStatusLevel(usage.remainingPercent), 'ok');
});

test('an unreported fraction and an unlimited window are not percentages', () => {
  assert.equal(formatQuotaUsage({ remaining_percentage: 0 }).kind, 'unavailable');
  assert.equal(formatQuotaUsage({ fraction_reported: true, unlimited: true }).kind, 'unlimited');
});

test('chunk mode still paints a nearly empty window, so it never reads as exhausted', () => {
  assert.deepEqual(chunkFill(0.01, 20), { full: 0, half: true });
  assert.deepEqual(chunkFill(0, 20), { full: 0, half: false });
  assert.equal(chunkFill(null, 20), null);
});

// Shape checks for the quota colour policy in panel/format.js.
//
// The quota page reads its colours from one place: quotaStatusLevel() picks a tone from what is
// REMAINING, and every bar, chip and card border consumes that tone. These checks pin the
// boundaries, because an off-by-one here silently paints a nearly-exhausted window green.
//
// Not covered here: the rendered DOM (chip strip, bars, countdowns) or the page's live account
// list. Those need a browser — open the Quota page and confirm the chips jump to their card, the
// bars fill from the left, the countdown sits at the right of each row, and the colour matches the
// percentage written beside the bar. The pure visibility and merge policies are covered below.
//
// Run: node --test panel/test/quota-display.test.mjs

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  QUOTA_STATUS_CRITICAL_AT_OR_BELOW,
  QUOTA_STATUS_HEALTHY_ABOVE,
  chunkFill,
  formatQuotaUsage,
  mergeQuotaRecord,
  quotaAccountVisible,
  formatResetIn,
  formatResetInLabel,
  quotaFamilyLabel,
  quotaStatusLevel,
  quotaWindows,
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

test('the countdown beside a percentage names itself and never invents a time', () => {
  const now = Date.UTC(2026, 0, 1, 12, 0, 0);
  assert.equal(formatResetInLabel(now + (6 * 86400 + 23 * 3600) * 1000, now), 'Resets in: 6 d 23 h');
  assert.equal(formatResetInLabel(now + 45_000, now), 'Resets in: 45 s');
  assert.equal(formatResetInLabel(now - 1, now), 'Resets now');
  assert.equal(formatResetInLabel(null, now), 'Reset time unknown');
  assert.equal(formatResetInLabel(undefined, now), 'Reset time unknown');
  // The sentence form keeps working: the same countdown, phrased for prose.
  assert.equal(formatResetIn(now + 45_000, now), 'resets in 45 s');
  assert.equal(formatResetIn(null, now), 'reset time unknown');
});

test('a provider that quantifies one window gets one row, not three padded ones', () => {
  const single = [{ family: '1w', fraction_reported: true, remaining_percentage: 100, total: 100 }];
  assert.equal(quotaWindows(single).length, 1);
  // Unlimited is an answer and keeps its row; a family the provider never mentioned does not appear.
  assert.equal(quotaWindows([{ unlimited: true, fraction_reported: true }]).length, 1);
  assert.deepEqual(quotaWindows(undefined), []);
  assert.deepEqual(quotaWindows([null, 'nope']), []);
});

test('silent windows are dropped, and the named families lead in 5h, 1w, 30d order', () => {
  const windows = [
    { family: '30d', fraction_reported: true, remaining_percentage: 78, total: 100 },
    { family: '1w', fraction_reported: true, remaining_percentage: 99.8, total: 100 },
    { family: { other: { secs: 3600 } }, fraction_reported: true, remaining_percentage: 4, total: 10 },
    { family: '5h' }, // no fraction: silence is not a measurement, so there is no row
  ];
  assert.deepEqual(quotaWindows(windows).map((window) => window.family),
    ['1w', '30d', { other: { secs: 3600 } }]);
});

test('a family the panel has no short name for still gets a label', () => {
  assert.equal(quotaFamilyLabel('1w'), '1w');
  assert.equal(quotaFamilyLabel({ other: { secs: 3600 } }), 'other (3600s)');
  assert.equal(quotaFamilyLabel({ other: {} }), 'other');
  assert.equal(quotaFamilyLabel(null), '');
  assert.equal(quotaFamilyLabel({}), '');
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

test('an error never creates a quota card; it sits on a card that has a window', () => {
  const quantified = { family: '1w', fraction_reported: true, remaining_percentage: 78, total: 100 };
  assert.equal(quotaAccountVisible({ windows: [quantified] }), true);
  assert.equal(quotaAccountVisible({ windows: [{ unlimited: true, fraction_reported: true }] }), true);

  // The 401 an account earns before it has ever produced a snapshot has no allowance to describe.
  assert.equal(quotaAccountVisible({ windows: [], error: 'rejected the API key (HTTP 401)', fetched_at: null }), false);
  assert.equal(quotaAccountVisible({ windows: [], error: 'rejected the API key (HTTP 401)' }), false);
  // Even a timestamp does not make an error-only record a card.
  assert.equal(quotaAccountVisible({ windows: [], error: 'refresh failed', fetched_at: 1 }), false);
  // Silence is still not a card: no window, no error.
  assert.equal(quotaAccountVisible({ windows: [], error: null, fetched_at: 1 }), false);
  // A window the provider never quantified is not a window (openrouter's unlimited-but-unreported
  // window must stay invisible even if a refresh later fails).
  assert.equal(quotaAccountVisible({ windows: [{ family: '1w', unlimited: true, fraction_reported: false }], error: 'failed', fetched_at: 1 }), false);
});

test('a failed refresh keeps the last-good window and its fetch time', () => {
  const previous = {
    provider: 'p', account: 'a', fetched_at: 100,
    windows: [{ family: '1w', fraction_reported: true, remaining_percentage: 60, total: 100 }],
  };
  const failed = { provider: 'p', account: 'a', fetched_at: null, windows: [], error: 'HTTP 401' };
  const merged = mergeQuotaRecord(previous, failed);
  assert.equal(merged.windows, previous.windows);
  assert.equal(merged.fetched_at, 100);
  assert.equal(merged.error, 'HTTP 401');
  assert.equal(quotaAccountVisible(merged), true);

  // A successful empty answer is news: the window is gone, so stale numbers must not survive it.
  const empty = { provider: 'p', account: 'a', fetched_at: 200, windows: [], error: null };
  assert.deepEqual(mergeQuotaRecord(previous, empty), empty);

  // A failed refresh that did return a window replaces the snapshot and its timestamp.
  const partial = { provider: 'p', account: 'a', fetched_at: 300, windows: [previous.windows[0]], error: 'partial' };
  assert.deepEqual(mergeQuotaRecord(previous, partial), partial);
});

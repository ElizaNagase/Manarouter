// What the sidebar's live combo block says.
//
// The rendering decision is a pure function of the payload and a clock, so all four states —
// in flight, idle with history, never run, control plane unreachable — are asserted here with
// plain node. The DOM half (panel/combo-live.js `startComboLive`) is a direct translation of the
// view it returns, so what is tested is what gets painted.
//
// Run: node --test panel/test/combo-live.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { comboActivityIndex, comboActivityView, comboAgeLabel, comboRoutePreview, recentCombos, startComboLive } from '../combo-live.js';

const T0 = 1_793_641_349_000;

const finished = (overrides = {}) => ({
  combo: 'expensivesmart',
  requested: 'expensivesmart',
  step: 'openai-codex/gpt-6-luna',
  started_at: T0,
  finished_at: T0 + 800,
  outcome: 'ok',
  ...overrides,
});

const inFlight = (overrides = {}) => ({
  combo: 'expensivesmart',
  requested: 'expensivesmart',
  step: 'openai-codex/gpt-6-luna',
  started_at: T0,
  finished_at: null,
  outcome: null,
  ...overrides,
});

const deepseekCombo = (overrides = {}) => ({
  name: 'deepseek',
  steps: [
    { provider: 'openadapter', model: 'DeepSeek-V4.1-Flash', enabled: true },
    { provider: 'moonmath', model: 'deepseek-v4.1-flash', enabled: true },
    { provider: 'tokenharbor', model: 'deepseek-v4.1-flash:free', enabled: true },
    { provider: 'commandcodego', model: 'deepseek-v4.1-flash', enabled: true },
    { provider: 'opencode-go', model: 'deepseek-v4.1-flash', enabled: true },
  ],
  ...overrides,
});

test('a request in flight shows the combo and the step the relay chose, labelled live', () => {
  const view = comboActivityView({ entries: [inFlight()] }, { now: T0 + 2_000 });
  assert.equal(view.state, 'live');
  assert.equal(view.combo, 'expensivesmart');
  assert.equal(view.step, 'openai-codex/gpt-6-luna');
  assert.equal(view.route, 'openai-codex/gpt-6-luna');
  assert.equal(view.status, 'live');
});

test('an in-flight request wins over a newer finished one', () => {
  // Two overlapping requests are possible: the newest entry is not necessarily the running one,
  // and the block answers "what is happening now" whenever anything is.
  const view = comboActivityView(
    { entries: [finished({ step: 'other/model' }), inFlight()] },
    { now: T0 + 1_000 },
  );
  assert.equal(view.state, 'live');
  assert.equal(view.step, 'openai-codex/gpt-6-luna');
});

test('nothing running shows the most recent decision and its age', () => {
  const view = comboActivityView({ entries: [finished()] }, { now: T0 + 12_000 });
  assert.equal(view.state, 'recent');
  assert.equal(view.route, 'openai-codex/gpt-6-luna');
  assert.equal(view.status, '12s ago');
});

test('a failed decision is still history, shown with its age', () => {
  const view = comboActivityView(
    { entries: [finished({ finished_at: T0 + 300, outcome: 'failed' })] },
    { now: T0 + 96_000 },
  );
  assert.equal(view.state, 'recent');
  assert.equal(view.status, '1m ago');
});

test('a relay that has never served a combo says so', () => {
  for (const payload of [{ entries: [] }, { entries: [] , cooldowns: {} }, {}]) {
    const view = comboActivityView(payload, { now: T0 });
    assert.equal(view.state, 'empty');
    assert.equal(view.status, 'no combo activity yet');
    assert.equal(view.route, '');
  }
});

test('an unreachable control plane is one short line, not an error dump', () => {
  const view = comboActivityView(null, { unreachable: true, now: T0 });
  assert.equal(view.state, 'unreachable');
  assert.equal(view.status, 'control plane unreachable');
  assert.equal(view.route, '');
  assert.ok(view.status.length < 40);
});

test('a malformed entry is not rendered as half a route', () => {
  const view = comboActivityView(
    { entries: [{ combo: 'expensivesmart' }, null, 'nonsense'] },
    { now: T0 },
  );
  assert.equal(view.state, 'empty');
});

test('the overview gets one box per combo used in the last five minutes', () => {
  const entries = [
    finished({ combo: 'cheap', started_at: T0 + 30_000, step: 'a/one' }),
    finished({ combo: 'cheap', started_at: T0 + 10_000, step: 'a/zero' }),
    finished({ combo: 'deepseek', started_at: T0 + 20_000, step: 'b/two' }),
  ];
  const list = recentCombos({ entries }, { now: T0 + 60_000 });
  assert.deepEqual(list.map((item) => item.combo), ['cheap', 'deepseek']);
  assert.equal(list[0].count, 2);
  assert.equal(list[0].step, 'a/one', 'the newest step for that combo is the one remembered');
  assert.equal(list[1].count, 1);
});

test('a combo is dropped from the overview once its last request leaves the window', () => {
  const entries = [
    finished({ combo: 'edge', started_at: T0 }),
    finished({ combo: 'old', started_at: T0 - 300_001 }),
    finished({ combo: 'future', started_at: T0 + 120_000 }),
  ];
  const list = recentCombos({ entries }, { now: T0 });
  assert.deepEqual(list.map((item) => item.combo), ['edge']);
});

test('a combo with a request in flight leads the overview row even if it started earlier', () => {
  const entries = [
    finished({ combo: 'recent', started_at: T0 + 10_000 }),
    inFlight({ combo: 'running', started_at: T0 + 1_000 }),
  ];
  const list = recentCombos({ entries }, { now: T0 + 20_000 });
  assert.deepEqual(list.map((item) => item.combo), ['running', 'recent']);
  assert.equal(list[0].live, true);
});

test('a request that is still in flight stays visible past the five-minute window', () => {
  const entries = [
    finished({ combo: 'recent', started_at: T0 + 10_000 }),
    inFlight({ combo: 'running', started_at: T0 - 600_000 }),
  ];
  const list = recentCombos({ entries }, { now: T0 + 20_000 });
  assert.deepEqual(list.map((item) => item.combo), ['running', 'recent']);
  assert.equal(list[0].live, true);
});

test('an activity payload that cannot be read yields no overview boxes rather than guesses', () => {
  for (const payload of [null, {}, { entries: [null, 'nonsense', { combo: 'x' }] }]) {
    assert.deepEqual(recentCombos(payload, { now: T0 }), []);
  }
});

// ---------------------------------------------------------------------------
// The overview tile's route: the relay's current decision when there is one,
// the configured starting point only when there is not.
// ---------------------------------------------------------------------------

test('a live combo names the step being served, not its configured first step', () => {
  const recent = recentCombos(
    { entries: [inFlight({ combo: 'deepseek', step: 'opencode-go/deepseek-v4.1-flash' })] },
    { now: T0 + 2_000 },
  )[0];
  const preview = comboRoutePreview(deepseekCombo(), recent);
  assert.equal(preview.route, 'opencode-go/deepseek-v4.1-flash');
  assert.notEqual(preview.route, 'openadapter/DeepSeek-V4.1-Flash');
  assert.equal(preview.live, true);
  assert.equal(preview.source, 'serving');
  assert.equal(preview.title, 'Serving now: opencode-go/deepseek-v4.1-flash');
});

test('a finished-but-recent combo still names the step that served it', () => {
  const recent = recentCombos(
    { entries: [finished({ combo: 'deepseek', step: 'tokenharbor/deepseek-v4.1-flash:free' })] },
    { now: T0 + 2_000 },
  )[0];
  const preview = comboRoutePreview(deepseekCombo(), recent);
  assert.equal(preview.route, 'tokenharbor/deepseek-v4.1-flash:free');
  assert.equal(preview.live, false);
  assert.equal(preview.source, 'serving');
  assert.equal(preview.title, 'Served recently: tokenharbor/deepseek-v4.1-flash:free');
});

test('an idle combo falls back to its first enabled step, labelled as the start', () => {
  const preview = comboRoutePreview(deepseekCombo());
  assert.equal(preview.route, 'openadapter/DeepSeek-V4.1-Flash');
  assert.equal(preview.live, false);
  assert.equal(preview.source, 'starts-at');
  assert.equal(preview.title, 'Starts at: openadapter/DeepSeek-V4.1-Flash');
});

test('an idle combo skips a disabled first step', () => {
  const combo = deepseekCombo({
    steps: [
      { provider: 'off', model: 'disabled-first', enabled: false },
      { provider: 'on', model: 'enabled-second', enabled: true },
    ],
  });
  const preview = comboRoutePreview(combo);
  assert.equal(preview.route, 'on/enabled-second');
  assert.equal(preview.source, 'starts-at');
});

test('a missing or non-string recent step falls back and never renders undefined', () => {
  for (const step of [undefined, null, 42, '', '   ']) {
    const preview = comboRoutePreview(deepseekCombo(), { combo: 'deepseek', step, live: false });
    assert.equal(preview.route, 'openadapter/DeepSeek-V4.1-Flash');
    assert.equal(preview.source, 'starts-at');
    assert.doesNotMatch(preview.title, /undefined/);
  }
  // A still-live entry with no reported step is honest about the gap instead of claiming the
  // configured first step is what is running.
  const live = comboRoutePreview(deepseekCombo(), { combo: 'deepseek', step: null, live: true });
  assert.equal(live.route, 'openadapter/DeepSeek-V4.1-Flash');
  assert.equal(live.title, 'Serving now: step not reported');
});

test('a combo with no usable steps yields one stable placeholder', () => {
  for (const combo of [
    { name: 'empty', steps: [] },
    { name: 'missing' },
    { name: 'malformed', steps: [{ provider: 'only-provider', enabled: true }] },
  ]) {
    const preview = comboRoutePreview(combo);
    assert.equal(preview.route, 'no steps configured');
    assert.equal(preview.source, 'starts-at');
    assert.equal(preview.title, 'Starts at: no steps configured');
  }
});

// The step a row names belongs to the request that decides the row's state. A newer *finished*
// request must not lend its route to a combo that is still serving, or the panel reports a step
// the relay has already left — the bug this pair of tests exists to keep out.
test('a running request older than a newer finished one still supplies the live step', () => {
  const entries = [
    finished({ combo: 'deepseek', step: 'openadapter/DeepSeek-V4.1-Flash', started_at: T0 + 5_000 }),
    inFlight({ combo: 'deepseek', step: 'opencode-go/deepseek-v4.1-flash', started_at: T0 }),
  ];
  const [row] = recentCombos({ entries }, { now: T0 + 6_000 });
  assert.equal(row.live, true);
  assert.equal(row.step, 'opencode-go/deepseek-v4.1-flash');
  const preview = comboRoutePreview(deepseekCombo(), row);
  assert.equal(preview.route, 'opencode-go/deepseek-v4.1-flash');
  assert.equal(preview.title, 'Serving now: opencode-go/deepseek-v4.1-flash');
});

test('overlapping runs name the newest running step, whichever order the ring reports', () => {
  const older = inFlight({ combo: 'deepseek', step: 'moonmath/deepseek-v4.1-flash', started_at: T0 });
  const newer = inFlight({ combo: 'deepseek', step: 'opencode-go/deepseek-v4.1-flash', started_at: T0 + 2_000 });
  for (const entries of [[newer, older], [older, newer]]) {
    const [row] = recentCombos({ entries }, { now: T0 + 3_000 });
    assert.equal(row.live, true);
    assert.equal(row.step, 'opencode-go/deepseek-v4.1-flash');
  }
});

test('a running request whose route the relay did not report is live without a route claim', () => {
  const [row] = recentCombos(
    { entries: [inFlight({ combo: 'deepseek', step: undefined })] },
    { now: T0 + 1_000 },
  );
  assert.equal(row.live, true);
  assert.equal(row.step, undefined);
  const preview = comboRoutePreview(deepseekCombo(), row);
  assert.equal(preview.route, 'openadapter/DeepSeek-V4.1-Flash');
  assert.equal(preview.title, 'Serving now: step not reported');
});

test('the combos list keys activity by combo id and keeps the running step', () => {
  const combos = [{ id: 'c1', name: 'deepseek' }, { id: 'c2', name: 'other' }];
  const index = comboActivityIndex(combos, [
    finished({ combo: 'deepseek', step: 'openadapter/DeepSeek-V4.1-Flash', started_at: T0 + 5_000 }),
    inFlight({ combo: 'deepseek', step: 'opencode-go/deepseek-v4.1-flash', started_at: T0 }),
    finished({ combo: 'other', step: 'x/y', started_at: T0 + 1_000 }),
    // A name no configured combo carries must not resurrect a deleted one.
    inFlight({ combo: 'deleted', step: 'gone/model', started_at: T0 + 9_000 }),
  ]);
  assert.equal(index.size, 2);
  const live = index.get('c1');
  assert.equal(live.live, true);
  assert.equal(live.step, 'opencode-go/deepseek-v4.1-flash');
  assert.equal(live.last, T0 + 5_000);
  const idle = index.get('c2');
  assert.equal(idle.live, false);
  // Idle means idle: a finished request supplies no serving step for the list to paint.
  assert.equal(idle.step, undefined);
  assert.equal(idle.last, T0 + 1_000);
});

test('a finished entry never supplies a serving step, even when it is the newest', () => {
  const index = comboActivityIndex(
    [{ id: 'c1', name: 'deepseek' }],
    [finished({ combo: 'deepseek', step: 'newer/model', started_at: T0 + 5_000 })],
  );
  assert.equal(index.get('c1').live, false);
  assert.equal(index.get('c1').step, undefined);
});

test('relative ages are stated in one unit that fits the sidebar', () => {
  assert.equal(comboAgeLabel(T0, T0), 'just now');
  assert.equal(comboAgeLabel(T0, T0 + 1_000), '1s ago');
  assert.equal(comboAgeLabel(T0, T0 + 59_000), '59s ago');
  assert.equal(comboAgeLabel(T0, T0 + 60_000), '1m ago');
  assert.equal(comboAgeLabel(T0, T0 + 3_600_000), '1h ago');
  assert.equal(comboAgeLabel(T0, T0 + 86_400_000), '1d ago');
  // A clock that went backwards must not produce a negative age.
  assert.equal(comboAgeLabel(T0, T0 - 5_000), 'just now');
  assert.equal(comboAgeLabel(null, T0), 'unknown age');
});

// ---------------------------------------------------------------------------
// The DOM half: startComboLive paints the view it was given.
//
// A ~20-line stub is enough because the block builds three elements and sets text on them. What
// this pins is that the payload reaches the sidebar as text (never as markup) and that a failed
// poll paints one line rather than leaving the previous state up as if it were still true.
// ---------------------------------------------------------------------------

function makeEl(tag) {
  return {
    tagName: String(tag).toUpperCase(),
    children: [],
    dataset: {},
    attrs: {},
    className: '',
    textContent: '',
    title: '',
    href: '',
    append(...kids) { this.children.push(...kids); },
    replaceChildren(...kids) { this.children = kids; },
    setAttribute(k, v) { this.attrs[k] = String(v); },
    getAttribute(k) { return Object.hasOwn(this.attrs, k) ? this.attrs[k] : null; },
  };
}

function makeDoc() {
  const mount = makeEl('div');
  const announce = makeEl('span');
  return {
    visibilityState: 'visible',
    mount,
    announce,
    getElementById: (id) => {
      if (id === 'combo-live') return mount;
      if (id === 'combo-live-announce') return announce;
      return null;
    },
    createElement: makeEl,
  };
}

function makeWin() {
  return { addEventListener() {}, removeEventListener() {} };
}

/** Wait until `done()` holds, or give up: the poller's tick is async. */
async function until(done) {
  for (let i = 0; i < 400; i += 1) {
    if (done()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('the block never painted');
}

test('the live state is painted as text, with the route and the word live', async () => {
  const doc = makeDoc();
  const ctl = startComboLive({
    api: { get: async () => ({ entries: [inFlight()] }) },
    doc,
    win: makeWin(),
    now: () => T0,
  });
  try {
    ctl.start();
    await until(() => doc.mount.children.length > 0);
  } finally {
    // Always stop: a live poller keeps a timer armed, and a failing assertion would leave the
    // test process waiting on it instead of reporting the failure.
    ctl.stop();
  }

  assert.equal(doc.mount.dataset.state, 'live');
  // The block is a deep link to the combo, and the state is not a third visible line: it stays in
  // the live region so the two readable lines are the combo and the route it chose.
  const [link] = doc.mount.children;
  assert.equal(link.tagName, 'A');
  assert.equal(link.href, '#/combos/expensivesmart');
  assert.match(link.attrs['aria-label'], /expensivesmart, live at openai-codex\/gpt-6-luna/);
  const [head, route] = link.children;
  assert.equal(head.children[0].textContent, 'expensivesmart');
  assert.equal(head.children[0].className, 'combo-live-combo');
  assert.equal(head.children[1].textContent, 'live');
  assert.equal(head.children[1].className, 'sr-only');
  assert.equal(route.textContent, 'openai-codex/gpt-6-luna');
  assert.equal(route.title, 'expensivesmart: openai-codex/gpt-6-luna');
});

test('a failed poll replaces the shown state with one short line', async () => {
  const doc = makeDoc();
  let call = 0;
  const ctl = startComboLive({
    api: {
      get: async () => {
        call += 1;
        if (call > 1) throw new Error('control plane down');
        return { entries: [finished()] };
      },
    },
    doc,
    win: makeWin(),
    intervalMs: 20,
    now: () => T0 + 12_000,
  });
  try {
    ctl.start();
    await until(() => doc.mount.dataset.state === 'recent');
    await until(() => doc.mount.dataset.state === 'unreachable');
  } finally {
    ctl.stop();
  }

  assert.equal(doc.mount.children.length, 1);
  assert.equal(doc.mount.children[0].className, 'combo-live-empty');
  assert.equal(doc.mount.children[0].textContent, 'control plane unreachable');
});

test('a change of state is announced once, not on every poll', async () => {
  // The block is repainted every poll and the age ticks every second. Announcing that would read
  // the same sentence over and over, so only the state and the route reach the live region.
  const doc = makeDoc();
  const ctl = startComboLive({
    api: { get: async () => ({ entries: [finished()] }) },
    doc,
    win: makeWin(),
    intervalMs: 10,
    now: () => T0 + 12_000,
  });
  try {
    ctl.start();
    await until(() => doc.announce.textContent !== '');
  } finally {
    ctl.stop();
  }

  const first = doc.announce.textContent;
  assert.equal(first, 'last combo decision expensivesmart: openai-codex/gpt-6-luna');
  // A second identical poll must not rewrite the announcement.
  ctl.paint(comboActivityView({ entries: [finished()] }, { now: T0 + 40_000 }));
  assert.equal(doc.announce.textContent, first);
});

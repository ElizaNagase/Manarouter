// combo-live.js — the live combo visualizer in the sidebar.
//
// The relay is the only process that knows which step a combo actually committed to, so this block
// is fed by GET /api/activity (the control plane proxies the relay's /control/activity ring). It
// answers one question, over and over: what is a combo doing right now, and if nothing is running,
// what did it do last and how long ago.
//
// The decision of what to show is `comboActivityView`, a pure function of the payload and a clock.
// Keeping it free of the DOM is what makes "in flight", "idle with history", "never run" and
// "control plane unreachable" testable with plain node (panel/README.md, "Testing the pure
// functions"); `startComboLive` only paints the result.
//
// Polling uses the shared scheduler, which already stops entirely while the tab is hidden. A
// sidebar block that kept waking a socket in a background tab for days would be exactly the waste
// this project exists to avoid.

import { api as defaultApi } from './api.js';
import { createPoller } from './poll.js';

/** "just now" | "12s ago" | "3m ago" | "2h ago" | "4d ago". */
export function comboAgeLabel(startedAt, now = Date.now()) {
  // Epoch milliseconds from the relay. A missing value is "unknown", not 1970: `Number(null)` is
  // zero, and rendering that as an age of fifty years would look like a working field.
  if (startedAt === null || startedAt === undefined || startedAt === '') return 'unknown age';
  const t = Number(startedAt);
  if (!Number.isFinite(t)) return 'unknown age';
  const secs = Math.max(0, Math.round((now - t) / 1000));
  if (secs < 1) return 'just now';
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/**
 * What the block should say, from the activity payload and the current time.
 *
 * States:
 *   live        — a combo request is being served; the combo is the heading, the route is the step alone, plus "live"
 *   recent      — nothing is running; the newest decision, with its age
 *   empty       — the relay is reachable and has never served a combo
 *   unreachable — the control plane (or the relay behind it) could not be asked
 *
 * A malformed entry counts as no decision rather than as a position to render: showing half a
 * route would be worse than showing none, and the payload is produced by our own process.
 */
export function comboActivityView(payload, { now = Date.now(), unreachable = false } = {}) {
  if (unreachable) {
    return {
      state: 'unreachable',
      combo: '',
      step: '',
      route: '',
      status: 'control plane unreachable',
      title: 'The panel could not ask the relay for combo activity.',
    };
  }

  const entries = Array.isArray(payload?.entries) ? payload.entries.filter(usable) : [];
  if (entries.length === 0) {
    return {
      state: 'empty',
      combo: '',
      step: '',
      route: '',
      status: 'no combo activity yet',
      title: 'No combo request has been served since the relay started.',
    };
  }

  // Newest first is the relay's order. An in-flight request wins over a newer finished one: two
  // requests can overlap, and "what is happening now" is the question this block exists for.
  const live = entries.find((entry) => entry.finished_at == null && entry.outcome == null) ?? null;
  const shown = live ?? entries[0];
  // The combo is already the heading of the block, so repeating it here would show the same name
  // twice ("expensivesmart expensivesmart → openai-codex/gpt-6-luna"). The route is the step alone;
  // the tooltip and the live-region sentence keep the combo because they are read out of context.
  const route = shown.step;
  return {
    state: live ? 'live' : 'recent',
    combo: shown.combo,
    step: shown.step,
    route,
    status: live ? 'live' : comboAgeLabel(shown.started_at, now),
    title: live
      ? `Serving ${shown.combo}: ${route}`
      : `Last combo decision: ${shown.combo}: ${route}`,
  };
}

/** An entry the overview may count: a named combo, or a still-running request whose step the
 * relay did not report (the run is real even when its route is missing, so it must not vanish). */
function reportable(entry) {
  if (!entry || typeof entry.combo !== 'string' || !entry.combo) return false;
  if (typeof entry.step === 'string' && entry.step) return true;
  return entry.finished_at == null && entry.outcome == null;
}

/**
 * The combos that saw a request in the last five minutes, one entry each.
 *
 * The sidebar answers "what is happening now"; the overview row answers "what has this router
 * been doing lately". Both read the same ring buffer, so this stays a pure function of the
 * payload and a clock: dedupe by combo, keep the newest start, count the calls, and put the
 * combo with a request in flight first.
 *
 * `step` is the step of the entry that decides the row's state: the newest still-running entry
 * when the combo is live, otherwise the newest started entry. A newer finished request must never
 * supply the step of a combo that is still live — that is how "LIVE NOW" came to name a step the
 * relay had already left.
 */
export function recentCombos(payload, { now = Date.now(), windowMs = 5 * 60_000 } = {}) {
  const entries = Array.isArray(payload?.entries) ? payload.entries.filter(reportable) : [];
  const seen = new Map();
  for (const entry of entries) {
    const startedAt = Number(entry.started_at);
    // A missing or nonsensical timestamp cannot be placed in the window, and a clock slightly
    // ahead of ours is treated as now rather than as a reason to hide a request that is running.
    if (!Number.isFinite(startedAt)) continue;
    const live = entry.finished_at == null && entry.outcome == null;
    // A still-running request belongs in the row however long ago it started: "live" is the one
    // thing this box exists to say. Finished entries are the ones the five-minute window bounds.
    if (!live && startedAt > now + 60_000) continue;
    if (!live && now - startedAt > windowMs) continue;
    const current = seen.get(entry.combo);
    if (!current) {
      seen.set(entry.combo, {
        combo: entry.combo,
        step: entry.step,
        count: 1,
        lastStartedAt: startedAt,
        live,
        liveStartedAt: live ? startedAt : null,
        stepStartedAt: startedAt,
      });
      continue;
    }
    current.count += 1;
    if (startedAt > current.lastStartedAt) current.lastStartedAt = startedAt;
    if (live) {
      current.live = true;
      // Two overlapping runs: the newest one is what the relay is serving, deterministically.
      // Equal timestamps keep the first seen, which is the relay's newest-first order.
      if (current.liveStartedAt === null || startedAt > current.liveStartedAt) {
        current.liveStartedAt = startedAt;
        current.step = entry.step;
      }
    } else if (current.liveStartedAt === null && startedAt > current.stepStartedAt) {
      // No running entry yet: the newest finished decision is the best step we have.
      current.step = entry.step;
      current.stepStartedAt = startedAt;
    }
  }
  const rows = [...seen.values()];
  for (const row of rows) {
    delete row.liveStartedAt;
    delete row.stepStartedAt;
  }
  return rows.sort((a, b) =>
    (Number(b.live) - Number(a.live))
    || (b.lastStartedAt - a.lastStartedAt)
    || a.combo.localeCompare(b.combo));
}

/**
 * Per-combo activity for the combos list, keyed by combo id: whether a request is running, the
 * newest start, and the step a running request is on.
 *
 * `step` exists because that list paints a "live" badge beside the route, and a live badge must
 * name the step being served, not the configured first step. As in `recentCombos`, the step is the
 * newest in-flight entry's, and only an in-flight entry can supply it: a request that already
 * finished must never be labelled as serving.
 *
 * Pure: no DOM. Entries whose combo is not in `combos` are ignored, so a stale name cannot
 * resurrect a deleted combo.
 */
export function comboActivityIndex(combos, entries) {
  const list = Array.isArray(combos) ? combos : [];
  const index = new Map();
  for (const entry of Array.isArray(entries) ? entries : []) {
    if (!entry || typeof entry.combo !== 'string') continue;
    const combo = list.find((c) => c && (c.name === entry.combo || c.id === entry.combo));
    if (!combo) continue;
    const current = index.get(combo.id)
      ?? { live: false, last: 0, step: undefined, liveStartedAt: null };
    const started = Number(entry.started_at);
    if (entry.finished_at == null && entry.outcome == null) {
      current.live = true;
      // Two overlapping runs: the newest one is what the relay is serving. An unusable timestamp
      // does not win over a usable one, and equal timestamps keep the first seen (relay order).
      if (current.liveStartedAt === null || !Number.isFinite(current.liveStartedAt)
        || (Number.isFinite(started) && started > current.liveStartedAt)) {
        current.liveStartedAt = started;
        current.step = entry.step;
      }
    }
    if (Number.isFinite(started)) current.last = Math.max(current.last, started);
    index.set(combo.id, current);
  }
  for (const state of index.values()) delete state.liveStartedAt;
  return index;
}

/** The tile's route when a combo has no usable step at all. */
const NO_STEPS = 'no steps configured';

/** "provider/model" for a configured step, or '' when either half is missing or not a string. */
function configuredStepRoute(step) {
  const provider = typeof step?.provider === 'string' ? step.provider.trim() : '';
  const model = typeof step?.model === 'string' ? step.model.trim() : '';
  return provider && model ? `${provider}/${model}` : '';
}

/**
 * What the overview tile should name as a combo's route, from the combo config and its recent
 * activity (one entry from `recentCombos`, or undefined).
 *
 * A running or recently-finished request is the truth about which step served; the configured
 * first step is only the starting point, and naming it while the relay is on step 5 is the lie
 * this function exists to remove. When there is no activity — or the activity did not report a
 * usable step — the configured first enabled step is shown, exactly as the tile did before.
 *
 * Pure: no DOM. `source` says which of the two meanings the route carries, so the view can keep
 * the tooltip honest without re-deriving the decision.
 */
export function comboRoutePreview(combo, recent = null) {
  const steps = Array.isArray(combo?.steps) ? combo.steps : [];
  // Preserve the historical idle choice: the first truthy `enabled`, else the first step.
  const firstStep = steps.find((step) => step?.enabled) ?? steps[0];
  const configuredRoute = configuredStepRoute(firstStep);
  const servedRoute = typeof recent?.step === 'string' ? recent.step.trim() : '';
  const serving = Boolean(servedRoute);
  const live = Boolean(recent?.live);
  const route = serving ? servedRoute : (configuredRoute || NO_STEPS);
  const title = serving
    ? (live ? `Serving now: ${route}` : `Served recently: ${route}`)
    : (live ? 'Serving now: step not reported' : `Starts at: ${route}`);
  return {
    route,
    live,
    source: serving ? 'serving' : 'starts-at',
    title,
  };
}

/** Whether an entry carries the two names the block renders. */
function usable(entry) {
  return Boolean(
    entry
    && typeof entry.combo === 'string' && entry.combo
    && typeof entry.step === 'string' && entry.step,
  );
}

/**
 * Mount the visualizer and return its controls.
 *
 * `doc` and `win` are injected so the poller can be exercised without a browser; `api` is injected
 * for the same reason. Nothing here touches the document until `start()` is called, so importing
 * this module in node tests is safe.
 */
export function startComboLive({
  api = defaultApi,
  doc = document,
  win = window,
  intervalMs = 1500,
  now = () => Date.now(),
} = {}) {
  const mount = doc.getElementById('combo-live');
  // Separate from `mount` on purpose: the block is repainted on every poll, and a live region that
  // is rewritten every 1.5 s repeats itself to anyone listening. This span changes only when the
  // state or the route changes, which is the thing worth announcing.
  const announcer = doc.getElementById('combo-live-announce');
  let started = false;
  let announced = null;

  /** Say the state out loud, once per actual change. */
  function announce(view) {
    if (!announcer) return;
    const text = view.state === 'live'
      ? `${view.combo}: ${view.route} is live`
      : view.state === 'recent'
        ? `last combo decision ${view.combo}: ${view.route}`
        : view.status;
    if (text === announced) return;
    announced = text;
    announcer.textContent = text;
  }

  function paint(view) {
    if (!mount) return;
    announce(view);
    mount.dataset.state = view.state;
    mount.replaceChildren();
    if (view.state === 'empty' || view.state === 'unreachable') {
      const line = doc.createElement('span');
      line.className = 'combo-live-empty';
      line.textContent = view.status;
      line.title = view.title;
      mount.append(line);
      return;
    }
    // The whole block is the link to the combo it is describing. The state ("live", an age, or
    // "control plane unreachable") stays in the live region instead of taking a visible slot: the
    // two lines an operator reads at a glance are the combo and the route it actually chose.
    const link = doc.createElement('a');
    link.className = 'combo-live-link';
    link.href = `#/combos/${encodeURIComponent(view.combo)}`;
    link.title = view.title;
    link.setAttribute('aria-label', view.state === 'live'
      ? `${view.combo}, live at ${view.route}. Open combo.`
      : `${view.combo}, last step ${view.route}, ${view.status}. Open combo.`);
    const head = doc.createElement('div');
    head.className = 'combo-live-head';
    const combo = doc.createElement('span');
    combo.className = 'combo-live-combo';
    combo.textContent = view.combo;
    combo.title = view.combo;
    const status = doc.createElement('span');
    status.className = 'sr-only';
    status.textContent = view.status;
    head.append(combo, status);
    const route = doc.createElement('div');
    route.className = 'combo-live-route';
    route.textContent = view.route;
    route.title = `${view.combo}: ${view.route}`;
    link.append(head, route);
    mount.append(link);
  }

  async function tick() {
    try {
      paint(comboActivityView(await api.get('/api/activity'), { now: now() }));
    } catch {
      // Any failure is one short line, never an error dump in the sidebar: the panel's toast and
      // the overdose of a stack trace both belong somewhere else.
      paint(comboActivityView(null, { unreachable: true }));
    }
  }

  const poller = createPoller({ tick, intervalMs, doc, win });

  return {
    start() {
      if (started || !mount) return;
      started = true;
      poller.start();
    },
    stop() {
      started = false;
      poller.stop();
    },
    get running() {
      return started;
    },
    /** Paint a known view, for a caller that already has the payload. */
    paint(view) {
      paint(view);
    },
  };
}

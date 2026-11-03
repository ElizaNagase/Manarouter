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
 *   live        — a combo request is being served; combo → step plus the word "live"
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
  const route = `${shown.combo} → ${shown.step}`;
  return {
    state: live ? 'live' : 'recent',
    combo: shown.combo,
    step: shown.step,
    route,
    status: live ? 'live' : comboAgeLabel(shown.started_at, now),
    title: live ? `Serving ${route}` : `Last combo decision: ${route}`,
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
      ? `${view.route} is live`
      : view.state === 'recent'
        ? `last combo decision ${view.route}`
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
    const head = doc.createElement('div');
    head.className = 'combo-live-head';
    const combo = doc.createElement('span');
    combo.className = 'combo-live-combo';
    combo.textContent = view.combo;
    combo.title = view.combo;
    const status = doc.createElement('span');
    status.className = 'combo-live-state';
    status.textContent = view.status;
    head.append(combo, status);
    const route = doc.createElement('div');
    route.className = 'combo-live-route';
    route.textContent = view.route;
    route.title = view.route;
    mount.append(head, route);
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

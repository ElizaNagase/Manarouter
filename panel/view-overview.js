// view-overview.js — health summary, counts, snapshot, relay reachability.
//
// Live updates arrive through ctx.onHealth (the 5 s poll in app.js). The relay probe is
// deliberately NOT on the poll loop: probing every tick would send requests to a relay nobody
// is looking at — the exact waste the poll scheduler exists to avoid. It runs once on open and
// on demand via the "Probe again" button.

import { api } from './api.js';
import { h, chip, spinner } from './dom.js';
import { formatUptime, formatAge, truncateMiddle, relayProbeUrl } from './format.js';
import { recentCombos } from './combo-live.js';

export async function renderOverview(mount, ctx) {
  const alive = { on: true };
  const root = h('div');
  mount.replaceChildren(root);

  const statGrid = h('div', { class: 'stat-grid' });
  const relayBox = h('div');
  // Below the relay boxes on purpose: reachability is the urgent fact, recent combo use is
  // context. The row is fed by the same health tick, never by a second poller of its own.
  const comboSection = h('section', { class: 'recent-combos', 'aria-label': 'Combos used in the last 5 minutes' });
  const comboHead = h('div', { class: 'recent-combos-head' },
    h('h2', {}, 'Combos'),
    h('span', { class: 'hint', 'data-combo-hint': '' }, 'last 5 minutes'));
  const comboRow = h('div', { class: 'combo-row', tabindex: '0' });
  comboSection.append(comboHead, comboRow);
  root.append(statGrid, relayBox, comboSection);

  function stat(k, valueEl, sub = null) {
    const v = h('div', { class: 'v' }, valueEl);
    const box = h('div', { class: 'stat' }, h('div', { class: 'k' }, k), v);
    if (sub) box.append(h('div', { class: 's' }, sub));
    return box;
  }

  function renderStats(health) {
    statGrid.replaceChildren();
    if (!health) {
      statGrid.append(h('div', { class: 'stat' },
        h('div', { class: 'k' }, 'control plane'),
        h('div', { class: 'v' }, 'unreachable'),
        h('div', { class: 's' }, 'no data is being invented — check the process log')));
      return;
    }
    const counts = health.counts ?? {};
    const digest = health.snapshot?.digest;
    statGrid.append(
      stat('version', h('span', { class: 'mono' }, String(health.version ?? '?'))),
      stat('uptime', h('span', { 'data-live': 'uptime' }, formatUptime(health.uptime_secs))),
      stat('snapshot', h('span', {
        class: 'mono',
        title: digest ? `digest ${digest}, built ${health.snapshot?.built_at ?? 'unknown'}` : 'no snapshot',
      }, digest ? truncateMiddle(digest) : 'none'),
        `built ${formatAge(health.snapshot?.built_at)}`),
      stat('providers', String(counts.providers ?? 0)),
      stat('combos', String(counts.combos ?? 0)),
      stat('aliases', String(counts.aliases ?? 0)),
      stat('client keys', String(counts.keys ?? 0)),
      stat('panel password',
        health.password_set ? chip('set', 'chip-green') : chip('not set', 'chip-amber'),
        health.password_set ? 'sessions require the password' : 'control plane has no password yet'),
    );
  }

  // --- combos used recently ------------------------------------------------

  let comboBusy = false;
  // Shared with restoreComboFocus: the box focused before this repaint, by combo name.
  let focusedCombo = null;

  function paintCombos(list, unreachable = false) {
    // The row is rebuilt on every health tick (5 s). Remember which box had focus so a keyboard
    // operator does not lose their place — and the :focus-within expansion — every five seconds.
    const doc = comboRow.ownerDocument;
    const active = doc?.activeElement ?? null;
    focusedCombo = active && active !== comboRow && comboRow.contains(active)
      ? active.dataset?.combo ?? null
      : null;
    comboRow.replaceChildren();
    if (unreachable) {
      comboRow.append(h('span', { class: 'combo-row-empty' },
        'Combo activity is unavailable right now; the relay status above is the place to look.'));
      comboHead.lastChild.textContent = 'last 5 minutes';
      restoreComboFocus();
      return;
    }
    if (!list || list.length === 0) {
      comboRow.append(h('span', { class: 'combo-row-empty' }, 'No combo has been used in the last 5 minutes.'));
      comboHead.lastChild.textContent = 'last 5 minutes';
      restoreComboFocus();
      return;
    }
    comboHead.lastChild.textContent = `last 5 minutes · ${list.length} combo${list.length === 1 ? '' : 's'}`;
    for (const item of list) {
      const when = item.live ? 'is live now' : `last used ${formatAge(item.lastStartedAt)}`;
      const box = h('button', {
        class: 'stat combo-stat',
        type: 'button',
        dataset: { combo: item.combo },
        title: `${item.combo} ${when}, ${item.count} call${item.count === 1 ? '' : 's'} in the last 5 minutes. Open combos.`,
      },
      h('div', { class: 'k' },
        `combo · ${item.live ? 'live' : formatAge(item.lastStartedAt)} · ${item.count} call${item.count === 1 ? '' : 's'}`),
      h('div', { class: 'v' }, item.combo));
      box.addEventListener('click', () => ctx.navigate('#/combos'));
      comboRow.append(box);
    }
    restoreComboFocus();
  }

  /** Put focus back on the same combo after a repaint, or on the row if it left the window. */
  function restoreComboFocus() {
    if (focusedCombo == null) return;
    const again = [...comboRow.querySelectorAll('.combo-stat')]
      .find((box) => box.dataset?.combo === focusedCombo);
    // `preventScroll` keeps a tick from yanking the page to a row the operator is already on.
    (again ?? comboRow).focus({ preventScroll: true });
  }

  async function refreshCombos() {
    if (!alive.on || !mount.isConnected || comboBusy) return;
    comboBusy = true;
    try {
      const payload = await api.get('/api/activity');
      if (alive.on) paintCombos(recentCombos(payload, { now: Date.now() }));
    } catch {
      if (alive.on) paintCombos(null, true);
    } finally {
      comboBusy = false;
    }
  }

  // --- relay reachability -------------------------------------------------

  async function probeRelay() {
    const url = relayProbeUrl(ctx.state.ports?.relay, globalThis.location?.hostname);
    relayBox.replaceChildren(h('div', { class: 'relay-up' }, h('h2', {}, 'Relay'), spinner(), ' probing…'));
    if (!url) {
      // Honest unknown: without a port from /api/ports there is nothing to dial.
      relayBox.replaceChildren(h('div', { class: 'relay-up' },
        h('h2', {}, 'Relay status unknown'),
        h('p', { class: 'hint' }, 'No relay listen address is available (GET /api/ports returned nothing usable), so the panel cannot probe it.')));
      return;
    }
    const controller = new AbortController();
    const bail = setTimeout(() => controller.abort(), 3000);
    let up = false;
    try {
      // mode:'no-cors' gives an opaque response: we cannot read status codes cross-origin, but a
      // resolve proves the socket answered. Reachability is all the panel claims here.
      await fetch(url, { mode: 'no-cors', cache: 'no-store', signal: controller.signal });
      up = true;
    } catch {
      up = false;
    } finally {
      clearTimeout(bail);
    }
    if (!alive.on) return; // view changed while probing; drop the result

    if (up) {
      relayBox.replaceChildren(h('div', { class: 'relay-up' },
        h('h2', {}, 'Relay reachable'),
        h('p', { class: 'hint' }, 'The data plane answered at ', h('span', { class: 'mono' }, url), '.'),
        probeButton()));
    } else {
      // The one big red surface in the panel: requests routed at the relay will fail right now.
      relayBox.replaceChildren(h('div', { class: 'relay-down' },
        h('h2', {}, 'Relay not reachable'),
        h('p', {}, 'Nothing answered at ', h('span', { class: 'mono' }, url), '. Requests forwarded to the relay will fail until it is running again.'),
        h('p', { class: 'hint' }, 'The control plane is fine — this is about the relay process or its listen port.'),
        probeButton()));
    }
  }

  function probeButton() {
    const b = h('button', { class: 'btn btn-sm', type: 'button' }, 'Probe again');
    b.addEventListener('click', () => probeRelay());
    return b;
  }

  // --- live health subscription --------------------------------------------

  ctx.onHealth((health) => {
    if (!alive.on) return;
    renderStats(health);
    refreshCombos();
  });

  await Promise.all([ctx.loadProviders().catch(() => null), ctx.refreshPorts()]);
  renderStats(ctx.state.health);
  refreshCombos();
  await probeRelay();
}

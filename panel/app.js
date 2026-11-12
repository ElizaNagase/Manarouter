// app.js — panel entry point: session gate, hash router, health polling, the persistent
// restart-pending banner, and the shared context (ctx) handed to every view.
//
// Structure: views live in ./view-*.js, each exporting `render(mount, ctx, param)`. ctx carries
// shared services (toast, confirm dialogs, cached data loaders, the restart flow) so views stay
// free of cross-imports.

import { api, ApiError, onUnauthorized } from './api.js';
import { h, clear, errorBlock, spinner } from './dom.js';
import { createPoller } from './poll.js';
import { createToasts } from './toast.js';
import { confirmDialog, promptDialog, secretKeyDialog } from './dialog.js';
import {
  extractList, portsDirty, describePortChange, truncateMiddle, formatUptime,
} from './format.js';

import { renderLogin } from './view-login.js';
import { renderOverview } from './view-overview.js';
import { renderProviders } from './view-providers.js';
import { renderCombos } from './view-combos.js';
import { renderKeys } from './view-keys.js';
import { renderAliases } from './view-aliases.js';
import { renderModels } from './view-models.js';
import { renderQuota } from './view-quota.js';
import { renderSettings } from './view-settings.js';
import { renderDocs } from './view-docs.js';
import { render as renderGpuRentals } from './view-gpu-rentals.js';
import { startComboLive } from './combo-live.js';

// ---------------------------------------------------------------------------
// Shared state & services
// ---------------------------------------------------------------------------

const state = {
  session: null,      // { authenticated, password_required }
  health: null,       // GET /api/health payload
  ports: null,        // GET /api/ports payload
  providers: null,    // cached GET /api/providers (models view, combo pickers, quota view)
  restarting: false,
};

const toast = createToasts(document.getElementById('toasts'));

// Load-once provider cache with in-flight de-duplication: several views want the same list and
// providers are the heaviest collection the control plane serves.
async function loadProviders(force = false) {
  if (!force && state.providers) return state.providers;
  if (state.providersPromise && !force) return state.providersPromise;
  state.providersPromise = api.get('/api/providers')
    .then((data) => {
      state.providers = extractList(data, 'providers');
      return state.providers;
    })
    .finally(() => { state.providersPromise = null; });
  return state.providersPromise;
}

// Live-update subscriptions. renderRoute() clears them before each view change, so a view's
// subscription dies with the view — no manual unsubscription bookkeeping, no leaks.
const healthSubs = new Set();
function emitHealth() {
  for (const cb of healthSubs) {
    // One view's failing callback must not starve the other subscribers or abort the poll tick.
    try { cb(state.health); } catch (err) { console.error('health subscriber failed', err); }
  }
}

async function refreshHealth() {
  const previous = state.health?.snapshot?.digest ?? null;
  try {
    state.health = await api.get('/api/health');
  } catch {
    state.health = null; // rendered as "control plane unreachable", never as fake data
  }
  // The control plane rebuilds its routing snapshot on every configuration write, so a new digest
  // means the provider list cached at page load no longer describes this router. Dropping it here
  // is what makes a provider added outside the panel (mr provider add, another tab) reach every
  // later read — the models view, the combo pickers, the quota view — instead of only the next
  // page load.
  if ((state.health?.snapshot?.digest ?? null) !== previous) state.providers = null;
  updateHealthPill();
  updateSidebarMeta();
  emitHealth();
}

async function refreshPorts() {
  try {
    state.ports = await api.get('/api/ports');
  } catch {
    state.ports = null;
  }
  updateBanner();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Shared restart flow (Settings view + the persistent banner button).
 *
 * WHY network errors are swallowed around POST /api/restart: restarting the control plane can
 * drop the very connection that carried the response — that is expected, not a failure. The
 * panel then polls /api/health until the process answers again (bounded to 30 s) and reports
 * honestly if it never comes back.
 */
async function restartFlow() {
  if (state.restarting) return; // a restart is already in flight — no double submit
  const ok = await confirmDialog(dialogRoot, {
    title: 'Restart Manarouter?',
    message: 'The relay and control plane will restart. In-flight requests may be dropped; the panel reconnects automatically.',
    confirmLabel: 'Restart',
    danger: false, // disruptive, not destructive — so the confirm button is pink, not red
  });
  if (!ok) return;
  if (state.restarting) return; // a second dialog was confirmed while this one was open

  state.restarting = true;
  updateHealthPill();
  updateBanner();
  try {
    await api.post('/api/restart');
  } catch (err) {
    if (!(err instanceof ApiError && (err.status === 0 || err.status >= 500))) {
      state.restarting = false;
      updateHealthPill();
      updateBanner();
      toast.error('Restart failed', err);
      return;
    }
    // Connection dropped or 5xx mid-restart: expected, keep waiting below.
  }

  let back = null;
  for (let i = 0; i < 30 && !back; i++) {
    await sleep(1000);
    try { back = await api.get('/api/health'); } catch { /* still coming up */ }
  }

  state.restarting = false;
  if (back) {
    state.health = back;
    toast.success('Restart complete.');
    // A refresh can still fail just after the port reopens; the panel must re-render anyway.
    await Promise.allSettled([refreshPorts(), loadProviders(true), refreshHealth()]);
    renderRoute();
  } else {
    updateHealthPill();
    updateBanner();
    toast.warning('The control plane has not answered for 30 s. It may still be restarting — check the process log.');
  }
}

const ctx = {
  state,
  leaveGuard: null, // a view with unsaved work sets this to an async () => boolean
  toast,
  confirm: (opts) => confirmDialog(dialogRoot, opts),
  prompt: (opts) => promptDialog(dialogRoot, opts),
  secretKey: (opts) => secretKeyDialog(dialogRoot, opts),
  // Programmatic navigation must ask the same unsaved-work guard the hash router asks, but before
  // the URL changes. Letting hashchange prompt after the fact meant a refused navigation had
  // already pushed a history entry, so Stay had to rewrite the URL and Back behaved oddly.
  navigate: async (hash) => {
    if (!hash || hash === location.hash) return;
    const guard = ctx.leaveGuard;
    if (!guard) { location.hash = hash; return; }
    if (navigationInFlight) return;
    navigationInFlight = true;
    try {
      let leave = false;
      try { leave = Boolean(await guard()); } catch { leave = false; }
      if (!leave) return;
      if (ctx.leaveGuard === guard) ctx.leaveGuard = null;
      location.hash = hash;
    } finally {
      navigationInFlight = false;
    }
  },
  loadProviders,
  refreshPorts,
  refreshHealth,
  restartFlow,
  onHealth: (cb) => {
    healthSubs.add(cb);
    if (state.health) cb(state.health); // immediate replay so late subscribers render once
    return () => healthSubs.delete(cb);
  },
};

// ---------------------------------------------------------------------------
// Login gate
// ---------------------------------------------------------------------------

const shell = document.getElementById('shell');
const loginRoot = document.getElementById('login-root');
const mainEl = document.getElementById('main');
const dialogRoot = document.getElementById('dialog-root');
const drawerRoot = document.getElementById('drawer-root');
const bannerSlot = document.getElementById('banner-slot');

// The sidebar's live combo block. Constructed once and started in enterApp(), so it exists for
// exactly as long as the shell is on screen — the login screen does not poll the relay.
const comboLive = startComboLive({ api });

function showLogin(notice = null) {
  poller.stop();
  comboLive.stop();
  ctx.leaveGuard = null;
  shell.hidden = true;
  loginRoot.hidden = false;
  clear(loginRoot);
  if (notice) toast.info(notice);
  renderLogin(loginRoot, {
    toast,
    onSuccess: async () => {
      try {
        state.session = await api.get('/api/session');
      } catch {
        state.session = { authenticated: true, password_required: true };
      }
      await enterApp();
    },
  });
  const first = loginRoot.querySelector('input');
  if (first) first.focus();
}

async function enterApp() {
  loginRoot.hidden = true;
  clear(loginRoot);
  shell.hidden = false;
  // Normalise the first URL without firing a second navigation. The guard is async and belongs to
  // the hashchange path; boot and explicit retries render directly.
  if (!location.hash) history.replaceState(null, '', '#/overview');
  renderRoute();
  await Promise.all([refreshHealth(), refreshPorts()]);
  poller.start();
  comboLive.start();
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

const ROUTES = [
  { id: 'overview', re: /^#\/overview$/, title: 'Overview', navLabel: 'Overview', navTitle: 'See gateway health and configured resource counts.', render: renderOverview },
  { id: 'providers', re: /^#\/providers$/, title: 'Providers', navLabel: 'Providers', navTitle: 'Manage upstream providers, accounts, and models.', render: renderProviders },
  { id: 'combos', re: /^#\/combos(?:\/(.+))?$/, title: 'Combos', navLabel: 'Combos', navTitle: 'Combine models into reusable routing presets.', render: renderCombos },
  { id: 'keys', re: /^#\/keys$/, title: 'Keys', navLabel: 'Keys', navTitle: 'Create and manage client API keys.', render: renderKeys },
  { id: 'aliases', re: /^#\/aliases$/, title: 'Aliases', navLabel: 'Aliases', navTitle: 'Create operator-authored model-name rewrites.', render: renderAliases },
  { id: 'models', re: /^#\/models$/, title: 'Models', navLabel: 'Models', navTitle: 'Browse available models and their capabilities.', render: renderModels },
  { id: 'quota', re: /^#\/quota$/, title: 'Quota', navLabel: 'Quota', navTitle: 'Review declared limits and observed usage.', render: renderQuota },
  { id: 'settings', re: /^#\/settings$/, title: 'Settings', navLabel: 'Settings', navTitle: 'Configure panel access, listen ports, and relay settings.', render: renderSettings },
  { id: 'docs', re: /^#\/docs(?:\/(.+))?$/, title: 'Documentation', navLabel: 'Documentation', navTitle: 'Read and download Manarouter documentation.', render: renderDocs },
  // The setup flow lives on a sub-path of the same route (`#/gpu-rentals/new/<step>`), so the
  // regex captures the step — the same shape the combos and docs routes use.
  { id: 'gpu-rentals', re: /^#\/gpu-rentals(?:\/(.+))?$/, title: 'GPU rentals', navLabel: 'GPU rentals', navTitle: 'Plan and track GPU capacity for self-hosted providers.', render: renderGpuRentals },
];

// Bumped on every route render: a stale view's late rejection must not touch the current route.
let routeSeq = 0;
// The hash whose view is actually on screen. A guard may hold a requested hash back until the
// operator answers, so location.hash alone is not a reliable description of the DOM.
let renderedHash = null;
let navigationInFlight = false;
let navigationRequestedAgain = false;

function currentHash() {
  return location.hash || '#/overview';
}

function renderRoute() {
  const gate = !state.session?.authenticated && state.session?.password_required;
  if (gate) { showLogin(); return;}

  const seq = ++routeSeq; // this render owns <main> until the next navigation
  healthSubs.clear(); // previous view's live subscriptions die here
  clear(drawerRoot);  // drawers are per-view UI state

  let hash = currentHash();
  let route = ROUTES.find((r) => r.re.test(hash));
  if (!route) {
    // An unknown hash (stale bookmark, typo) renders the overview. Rewrite the URL to match what
    // is on screen so the sidebar highlights Overview and a reload does not fall back silently again.
    hash = '#/overview';
    history.replaceState(null, '', hash);
    route = ROUTES[0];
  }
  renderedHash = hash;
  const m = hash.match(route.re);
  let param = null;
  if (m && m[1]) {
    // The hash is attacker-controllable (a shared link). A malformed %-escape must fall back to
    // the raw segment; throwing here escapes the router and leaves "Loading..." up forever.
    try { param = decodeURIComponent(m[1]); } catch { param = m[1]; }
  }

  document.title = `Manarouter — ${route.title}`;
  for (const a of document.querySelectorAll('#nav-list a')) {
    const navRoute = ROUTES.find((r) => r.id === a.dataset.route);
    if (navRoute) {
      a.textContent = navRoute.navLabel;
      a.title = navRoute.navTitle;
    }
    if (a.dataset.route === route.id) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }

  // Each route owns a fresh wrapper, not <main> itself. A view that paints after an await (quota
  // refreshes its cards, for example) then replaces children on its own detached wrapper instead
  // of erasing the page the operator has since navigated to. `seq` below still guards errors.
  // The page header is a sibling of the view, not a child: every view starts by clearing its own
  // wrapper, so a header inside it would be erased the moment the view painted.
  const head = h('header', { class: 'page-head' },
    h('h1', { class: 'page-title' }, route.title),
    route.navTitle ? h('p', { class: 'page-sub' }, route.navTitle) : null);
  const view = h('div', { class: 'route-view' },
    h('div', { class: 'toolbar' }, spinner(), h('span', { class: 'count' }, 'Loading…')));
  mainEl.replaceChildren(head, view);
  Promise.resolve(route.render(view, ctx, param)).catch((err) => {
    if (seq !== routeSeq) return; // a newer route owns <main>; its error screen is not ours to show
    clear(view);
    const box = errorBlock(err);
    const retry = h('button', { class: 'btn', type: 'button' }, 'Retry');
    retry.addEventListener('click', () => renderRoute());
    box.append(h('div', { class: 'dialog-actions' }, retry));
    view.append(box);
  });
}

/**
 * Apply a hash change, asking the current view before discarding unsaved work.
 *
 * The browser changes location.hash before it fires hashchange, so this cannot veto the URL
 * synchronously. It asks asynchronously and, on refusal, puts the rendered hash back with
 * replaceState (which does not fire another hashchange). On acceptance the guard is cleared before
 * rendering so the next page cannot inherit a prompt for a view that is already gone.
 */
async function handleHashChange() {
  if (shell.hidden) return; // login owns the screen
  if (navigationInFlight) {
    navigationRequestedAgain = true; // coalesce rapid clicks/back into one final route
    return;
  }
  navigationInFlight = true;
  try {
    do {
      navigationRequestedAgain = false;
      if (currentHash() === renderedHash) continue;

      const guard = ctx.leaveGuard;
      if (guard) {
        // A dialog is already asking something (the combo editor's own close confirmation, for
        // example). Opening the page guard on top would stack two modals and leave a ghost when the
        // first is dismissed. Treat this transition as refused and put the URL back; the operator
        // finishes the question in front of them first.
        if (dialogRoot.childElementCount > 0) {
          if (renderedHash) history.replaceState(null, '', renderedHash);
          navigationRequestedAgain = false;
          continue;
        }
        let leave = false;
        try { leave = Boolean(await guard()); } catch { leave = false; }
        if (!leave) {
          // Stay on the rendered view: restore the URL the DOM actually represents. Any second
          // navigation attempted while the dialog was open is discarded too.
          if (renderedHash) history.replaceState(null, '', renderedHash);
          navigationRequestedAgain = false;
          continue;
        }
        if (ctx.leaveGuard === guard) ctx.leaveGuard = null;
      }
      renderRoute();
    } while (navigationRequestedAgain || currentHash() !== renderedHash);
  } finally {
    navigationInFlight = false;
  }
}

// ---------------------------------------------------------------------------
// Global chrome: health pill, sidebar meta, restart-pending banner
// ---------------------------------------------------------------------------

function updateHealthPill() {
  const pill = document.getElementById('topbar-health');
  const dot = pill.querySelector('.dot');
  const text = pill.querySelector('.health-text');
  if (state.restarting) {
    dot.className = 'dot dot-amber';
    text.textContent = 'restarting…';
    pill.setAttribute('aria-label', 'Control plane is restarting');
    return;
  }
  if (!state.health) {
    dot.className = 'dot dot-red';
    text.textContent = 'control plane unreachable';
    pill.setAttribute('aria-label', 'Control plane unreachable — open overview');
    return;
  }
  dot.className = 'dot dot-green';
  // The version lives once, on the Overview; the top bar only answers "is the control plane up,
  // and for how long". Repeating the version in three places was noise, not reassurance.
  text.textContent = `healthy · up ${formatUptime(state.health.uptime_secs)}`;
  pill.setAttribute('aria-label', `Control plane healthy, version ${state.health.version ?? 'unknown'} — open overview`);
}

function updateSidebarMeta() {
  const el = document.getElementById('sidebar-meta');
  el.replaceChildren(
    state.health
      ? `v${state.health.version ?? '?'}`
      : 'disconnected',
  );
}

/**
 * The restart-pending banner. Amber by design: the saved port change is a warning that the
 * configuration is not live yet, not an error. It is rendered above <main> (outside the view)
 * so it persists across every view until the restart actually happens — there is no dismiss
 * button on purpose.
 */
function updateBanner() {
  clear(bannerSlot);
  const dirty = portsDirty(state.ports);
  const flagged = Boolean(state.ports?.restart_pending);
  if (!dirty && !flagged) return;

  const changes = describePortChange(state.ports);
  const box = h('div', { class: 'banner', role: 'status' });
  const text = h('div', { class: 'banner-text' });
  text.append(h('strong', {}, 'Pending restart: '));
  text.append(document.createTextNode(
    changes.length ? changes.join('; ') + '. ' : 'Saved port changes are not live yet. ',
  ));
  text.append(document.createTextNode('They apply on the next restart.'));
  box.append(text);

  if (state.restarting) {
    box.append(h('span', { class: 'count' }, 'restarting…'));
  } else {
    const btn = h('button', { class: 'btn btn-sm', type: 'button' }, 'Restart now');
    btn.addEventListener('click', () => restartFlow());
    box.append(btn);
  }
  bannerSlot.append(box);
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

const poller = createPoller({ tick: refreshHealth, intervalMs: 5000 });

// A 401 from any authenticated call lands here exactly once, centrally (see api.js).
onUnauthorized(() => {
  state.session = { authenticated: false, password_required: state.session?.password_required ?? true };
  // Concurrent 401s (a view fires several requests at once) must not rebuild the sign-in form
  // the user is already typing into.
  if (!loginRoot.hidden) return;
  showLogin('Your session has expired — please sign in again.');
});

// There is no log-out button in the shell any more (see index.html): the panel is a local
// single-operator console. The session still ends server-side, and a 401 from any call lands in
// the handler above and returns to the sign-in screen — that path is unchanged.

document.getElementById('topbar-health').addEventListener('click', () => {
  location.hash = '#/overview';
});

// Sidebar and in-page links are plain anchors: the browser would move the URL before any guard
// could run. Intercept them only when a view has unsaved work, and route them through navigate()
// so the prompt happens first and a refusal leaves no stray history entry.
document.addEventListener('click', (e) => {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  if (!ctx.leaveGuard || !shell.contains(e.target)) return;
  const a = e.target.closest?.('a[href^="#/"]');
  if (!a || a.target === '_blank' || a.hasAttribute('download')) return;
  e.preventDefault();
  void ctx.navigate(a.getAttribute('href'));
});

window.addEventListener('hashchange', () => { void handleHashChange(); });

// One global unload guard, governed by the same flag the router uses. Keeping it here (instead of
// in each view) means a view that is removed, saved, or reverted cannot leave a stale prompt behind.
window.addEventListener('beforeunload', (e) => {
  if (!ctx.leaveGuard) return;
  e.preventDefault();
  e.returnValue = '';
});

function showBootError(err) {
  // Never leave both roots hidden: even an unexpected error during login/app startup must
  // produce a visible, actionable screen rather than a blank dark page.
  poller.stop();
  ctx.leaveGuard = null;
  shell.hidden = false;
  loginRoot.hidden = true;
  clear(mainEl);
  const box = errorBlock(err);
  const retry = h('button', { class: 'btn btn-primary', type: 'button' }, 'Retry');
  retry.addEventListener('click', () => location.reload());
  box.append(h('div', { class: 'dialog-actions' }, retry));
  mainEl.append(box);
  document.getElementById('topbar-title').textContent = 'Manarouter';
}

(async function boot() {
  try {
    state.session = await api.get('/api/session');
    if (state.session.password_required && !state.session.authenticated) showLogin();
    else await enterApp();
  } catch (err) {
    // Covers both a failed session request and errors later in login/app startup.
    showBootError(err);
  }
})();

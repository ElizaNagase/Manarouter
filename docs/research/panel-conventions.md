# Panel conventions — implementation manual for the Manarouter operator panel

Source of truth read for this document: `panel/README.md`, `docs/15-web-panel.md`, `panel/{index.html,app.js,api.js,dom.js,toast.js,dialog.js,search-select.js,format.js,poll.js}`, `panel/view-{overview,keys,quota,aliases,gpu-rentals}.js`, `panel/test/*.test.mjs`, `crates/manarouter-conductor/src/{api.rs,serve.rs,state.rs,quota_api.rs,lib.rs,main.rs}`, `docs/{LLM_INDEX.md,23-implementation-status.md,15-web-panel.md}`.

The repo already contains a client-side `gpu-rentals` stub (`panel/view-gpu-rentals.js`, `panel/test/gpu-rentals.test.mjs`) registered in `app.js:254` and `index.html:57`, but it is localStorage-only and wired to `renderProviderWizardRegistry` — it has **no backend API**. Section 6 is the end-to-end checklist for making it real with 3 new conductor endpoints.

---

## 1. Design philosophy (quoted, with file:line)

### Brutalist / dense / utilitarian

`panel/README.md:3-5`: "plain HTML, CSS and ES modules ... No build step, no framework, no CDN, no external fonts ... must work on a fully offline install."

`docs/15-web-panel.md:9`: "That separation is the one architectural rule the panel must never break." — the panel runs on the control plane (20901), never the relay (20900); "the worst a broken panel can do is fail to render ... neither touches a token of anyone's inference" (`docs/15-web-panel.md:21-22`).

`docs/15-web-panel.md:254-255`: "It holds no more history than it displays, it does no work while no one is looking, and it starts no timers it cannot stop."

### Colour semantics

`docs/15-web-panel.md:138-143`: "Greys carry all the structure. A single accent is used for emphasis and interaction. The accent is never used for destructive actions or errors."

`panel/README.md:14`: app.css — "Greys carry structure; pink (`--pink`) marks interaction/emphasis only; green/amber/red are strictly semantic."

`docs/15-web-panel.md:147-152`:
| Green `#4cc38a` | Healthy, ready, enabled |
| Amber `#d9a441` | Degraded, cooling down, action needed |
| Red `#ef6a6a` | Broken, refused, destructive |

Consequences (`docs/15-web-panel.md:154-162`): "The accent is never red. A destructive button ... is red, and it is still secondary to the primary action." / "A semantic colour is never decorative."

`panel/app.css:7`: "Pink is never destructive and never an error; red is never decorative."

Semantic cases to copy (`panel/README.md:80-90`):
- provider with every account disabled → neutral grey, intentional state (`deriveProviderHealth`, `format.js:122`)
- pending restart → amber, a warning not an error; the restart banner has no dismiss button on purpose (`app.js:419-443`, `docs/15-web-panel.md:182-184`)
- restart confirm button → pink, "disruptive, not destructive" (`app.js:111`)
- red → genuinely dead states and destructive actions (delete provider/key/alias/model/step) and the big "Relay not reachable" surface on Overview (`panel/README.md:86-87`)
- model source chips (synced/manual/inferred) → neutral, provenance not health (`panel/README.md:88`)
- pink appears on: active nav item (`app.css:136-140`), focus rings (`app.css:56-60`), primary buttons (`app.css:520`), selected rows (`app.css:413`), links, priority-order callout

`format.js:75-76`: "GREEN = genuinely healthy · AMBER = warning/degraded/pending · RED = error/dead only · neutral grey for intentional states."

### Typography, spacing

`panel/app.css:50`: body font `13px/1.45 var(--sans)`; `:root` tokens at `panel/app.css:11-38`:
- `--bg0 #101314` page, `--bg1 #16191b` cards/sidebar, `--bg2 #1d2123` inputs/table head/code, `--bg3 #262b2e` hover, `--line #2b3134`, `--line2 #3a4145`
- `--tx1 #e3e7e8`, `--tx2 #9ba5a9`, `--tx3 #6b7579`
- `--pink #e7729f`, `--pink-dim`, `--pink-deep #cf4f85`, `--on-pink #180a11`
- `--green #4cc38a`, `--amber #d9a441`, `--red #ef6a6a` (each with `-dim` variant)
- `--sans` system stack; `--mono` ui-monospace stack for keys/ids/digests/model ids (`panel/app.css:34-35`, `panel/README.md:160`)
- `--radius: 6px` (`panel/app.css:38`)

### No-build-step / no-CDN constraints

`panel/README.md:5`: no build step, no framework, no CDN, no external fonts; files served straight from disk; relative `/api/…` requests only (`panel/README.md:7`).
`docs/15-web-panel.md:267-271`: no JSX/TypeScript/SFC; edit-and-reload; works offline.
`docs/15-web-panel.md:273-275`: panel served from the `panel/` directory found once at startup (cwd, parents, exe dir); missing `panel/` degrades to a placeholder page, never a crash (`serve.rs:125-131`).
`serve.rs:156-163`: static assets served with `Cache-Control: no-cache` so an edit reaches the tab on a plain reload.

### "No innerHTML except one sanctioned place"

`panel/dom.js:1-8`: "the only place this panel builds DOM ... Exactly one deliberate `innerHTML` — docs markdown in `view-docs.js`, escaped at the door by `markdown.js` — and server strings ... enter the DOM as text nodes or `textContent` only."
`panel/dom.js:17-32`: `h(tag, attrs, ...children)` — strings become text nodes; classes, dataset, aria/data, `on*` listeners, boolean attrs. `appendChildren` flattens arrays and skips null/false/''.
`panel/view-docs.js:32` (per README): renders doc text as markdown into `innerHTML` through `markdown.js`, which escapes `&<>"'` first.

Hard "do nots":
1. **Do not `innerHTML`** anything that came from the server (provider labels, model ids, error messages, doc text) — use `h()` / `textContent` (`dom.js:1-8`).
2. **Do not paint a destructive button pink** — red, secondary (`docs/15-web-panel.md:154-158`).
3. **Do not use a semantic colour decoratively** — green always means healthy (`docs/15-web-panel.md:160-162`).
4. **Do not poll while the tab is hidden** — `poll.js:21-22,45-49`; `panel/README.md:19`.
5. **Do not sort combo steps** — array order *is* priority order (`format.js:208-210`; `docs/15-web-panel.md:61`).
6. **Do not add cookie/web-session auth** — prohibited (`LLM_INDEX.md:248-249`, `docs/21-security.md`).
7. **Do not call the conductor from the relay** — zero IPC on the request path (`LLM_INDEX.md:357-358`).
8. **Do not leave a stale `leaveGuard`** — the global `beforeunload` guard in `app.js:484-488` is governed by the same flag views set (`ctx.leaveGuard`); clear it on save/delete/revert (`view-keys.js:220,276`).

### Keyboard / a11y requirements

`panel/README.md:161-163`: skip link, visible pink `:focus-visible` rings, focus-trapped dialogs with focus restore, `aria-label`s on every icon-only button, `aria-current` on nav/selection, tables use `scope="col"`, rows openable via keyboard.
`panel/README.md:164`: toasts are a live region (`aria-live="polite"`, errors `role="alert"`).
`dialog.js:1-6`: every dialog — role="dialog" + aria-modal, labelled by its title; focus moves in on open and returns to the opener on close; Tab trapped; Escape cancels. `confirmDialog:88-118` pre-focuses Cancel on danger confirms ("the safe choice").
`dom.js:192-205`: the "?" help affordance (`helpAffordance`) is a `<button>` with `data-tip` + a `.sr-only` span wired via `aria-describedby` — never a hover-only `title`.

### One screen = one job

`docs/15-web-panel.md:30-40`: each view has one purpose (Overview = health/counts, nothing can change; Providers = add/edit/remove; Quota = "how much is left" allowance first, usage ledger underneath; etc.)
`panel/app.js:309-310`: "The page header is a sibling of the view, not a child" — `renderRoute` builds `page-head` + `route-view`; views own their wrapper.

### Progressive disclosure

`docs/15-web-panel.md:25-26` (Providers): "an inline detail panel under the selected row: accounts ..., models ..., timeouts, headers, route overrides, per-provider Sync models" — detail expands in place under the row, not a separate page.
`panel/README.md:26` (Combos): "a step may point at another combo (nested routes expand in place; cycles are refused)."
Inline expansion is preferred over modals; modals/drawers are for creation, secrets shown once, and destructive confirmation.

### Table affordances

`panel/app.css:382-421`: `table.tbl` with sticky header (`position: sticky; top: 0`), `text-align: left`, uppercase 11px `--tx3` header, `.tbl .num` right-aligned tabular-nums, `.tbl .actions` right-aligned nowrap, hover row `--bg3`, `.row-selected` pink-dim (`panel/app.css:413`).
Rows openable via keyboard: `view-keys.js:54` uses `<button>` rows actions; `view-overview.js:224-229` makes combo tiles `<button type="button">` with `onclick`.
`.table-scroll` (`panel/app.css:371-380`): a wide table scrolls inside its own pane so it never widens the document on phone (`panel/README.md:131`).

### Empty states

`dom.js:355-357`: `emptyState(title, hint)` → `<div class="empty-state">`.
`panel/app.css:1146-1153`: dashed border, centred, `--tx2`, hint in `--tx3`.
Used in `view-keys.js:48-50`, `view-quota.js:158-159`, `view-aliases.js:90`, `view-overview.js:212`.

### Destructive confirmations

`dialog.js:88-118`: `confirmDialog(root, {title, message, warnLines=[], confirmLabel, cancelLabel, danger})` resolves true only on confirm; danger → `btn-danger` red (never pink), cancel pre-focused.
`view-keys.js:266-281`: delete key — confirm first, then `api.del`, toast success, reload.
`view-aliases.js:99-113`: same pattern.
Red = destructive (`panel/README.md:86`): `btn-danger` `panel/app.css:524-525`.

### Unsaved-edit guards

`docs/15-web-panel.md:78-87`: combo editor tracks dirty; route change asks "Stay on this page" / "Leave and discard"; URL restored on stay; `beforeunload` also blocked while dirty; saving/deleting/reverting clears the guard.
`app.js:154`: `ctx.leaveGuard = null` — a view with unsaved work sets it to `async () => boolean`.
`app.js:162-177`: `ctx.navigate(hash)` asks the guard before changing `location.hash`.
`app.js:336-375`: `handleHashChange` asks the guard, coalesces rapid navigations, restores URL on refusal (`history.replaceState`).
`app.js:471-478`: clicks on `a[href^="#/"]` are intercepted when `ctx.leaveGuard` is set and routed through `navigate()`.
`app.js:484-488`: one global `beforeunload` guard governed by `ctx.leaveGuard`.
Example guard pattern (from view-combos per docs; see also `view-gpu-rentals.js:192-199` remove-confirm):
```js
ctx.leaveGuard = async () => { /* return true if OK to leave */ };
```

### Toast usage

`panel/toast.js:9-47`: `createToasts(container)` → `{success, error, warning, info}`; TTLs success/info 6s, warning 9s, error 10s; error toasts render the backend's full `details` list (`toast.js:19-22`); errors `role="alert"`, others `role="status"`; stack capped at 6 (`toast.js:31-32`).
`panel/README.md:21`: "error toasts render the backend's full `details` list."

---

## 2. The CSS system

### Token names (`panel/app.css:11-38`)

Structure greys: `--bg0, --bg1, --bg2, --bg3, --line, --line2, --tx1, --tx2, --tx3`.
Accent: `--pink, --pink-dim, --pink-deep, --on-pink`.
Semantic: `--green, --green-dim, --amber, --amber-dim, --red, --red-dim`.
Stacks: `--sans, --mono`. Radius: `--radius: 6px`.

### Utility / component class names a new view must use

| Class | File:line | One-line usage |
|---|---|---|
| `card` | app.css:355-341 | Section card: `--bg1`, 1px line border, 6px radius, 12px padding |
| `grid-2` | app.css:351 | 2+ column responsive grid, minmax(240px,1fr), gap 12 |
| `stat-grid`, `stat` | app.css:353-367 | KPI strip; `.k` label, `.v` value, `.s` sub |
| `table-scroll` | app.css:371-380 | Overflow container so tables never widen the viewport |
| `table.tbl`, `.tbl th`, `.tbl td` | app.css:382-408 | Dense table, sticky header, left-aligned |
| `.tbl .num` | app.css:416 | Right-aligned numeric cell |
| `.tbl .actions` | app.css:420-421 | Right-aligned action buttons, nowrap |
| `chip`, `chip-green/amber/red/pink/neutral` | app.css:465-480 | Inline pill; semantic colours only |
| `button.chip`, `.chip-active` | app.css:487-500 | Toggle chip whose selected state is the pink fill |
| `btn`, `btn-primary`, `btn-danger`, `btn-ghost`, `btn-sm`, `btn-test` | app.css:504-538 | Buttons; primary = pink fill, danger = red outline (never pink fill) |
| `icon-btn`, `svg.i` | app.css:541-558 | Icon-only buttons (need `aria-label`) |
| `spin` | app.css:560-561 | Spinner animation on `icon('sync')` |
| `field`, `.input`, `.input.mono`, `textarea.input` | app.css:565-583 | Labelled input; `.input[aria-invalid="true"]` red border |
| `secret-field` | app.css:587-589 | Bordered wrapper for secret inputs |
| `hint` | app.css:591-592 | Muted helper text under a control |
| `help-dot`, `label-line` | app.css:599-674 | Keyboard-reachable "?" help with measured fixed bubble |
| `sr-only` | app.css:676-687 | Visually hidden text for screen readers |
| `field-error` | app.css:689-693 | Validation message pinned next to an input |
| `form-errors`, `form-ok` | app.css:695-716 | Red error block / green confirm block |
| `fieldset.hedge`, `fieldset.plain` | app.css:719-732 | Grouped fieldsets with small-caps legend |
| `check-row` | app.css:736-742 | Checkbox row whose label is the click target |
| `switch` | app.css:766-794 | On/off switch over a real checkbox |
| `grid-fields` | app.css:796 | Grid of fields, minmax(150px,1fr) |
| `banner` | app.css:1029-1040 | Amber pending-restart banner (never red) |
| `callout` | app.css:1044-1053 | Pink left-border emphasis box |
| `relay-down`, `relay-up` | app.css:1056-1071 | The one red surface (down) vs green (up) |
| `empty-state` | app.css:1146-1153 | Dashed centred empty illustration |
| `drawer`, `drawer-head`, `drawer-body` | app.css:1356-1379 | Fixed right drawer (z-index 40) |
| `dialog-backdrop`, `dialog`, `dialog-head`, `dialog-body`, `dialog-actions` | app.css:1393-1426 | Modal dialog (z-index 60) |
| `copy-target`, `copy-row`, `copy-ok` | app.css:1435-1442 | Copy affordance; `.copy-ok` flashes green |
| `toasts`, `toast`, `toast-success/error/warning/info` | app.css:1471-1502 | Toast region and semantic toast borders |

### Which stylesheet a new view uses, and how that is decided

`panel/index.html:12-17` load order:
```html
<link rel="stylesheet" href="./app.css">
<link rel="stylesheet" href="./lab.css">
<link rel="stylesheet" href="./lab-providers.css">
<link rel="stylesheet" href="./lab-models.css">
<link rel="stylesheet" href="./lab-combos.css">
<link rel="stylesheet" href="./lab-quota.css">
```

Split decision:
- `app.css` — the shared design system: all `--*` tokens plus every shared component class above (cards, fields, buttons, chips, tables, dialogs, toasts, empty states, banners). A new view **must** import nothing extra to use these; `app.css` is already loaded by every page.
- `lab.css` — shared layout/typography overrides for the "lab" layer (shell, top bar, page, buttons, fields, cards, tables, dialogs, responsive). Loaded after `app.css`; overrides, never rewrites (`lab.css:1-10`).
- `lab-providers.css`, `lab-models.css`, `lab-combos.css`, `lab-quota.css` — view-specific styles for the four heavy views.

Rule for a new view: put shared tokens/components in `app.css` (or reuse existing classes); only create `lab-<view>.css` when the view needs layout that would pollute the shared system, and add `<link rel="stylesheet" href="./lab-<view>.css">` to `index.html` **after** `lab.css` and after any other `lab-*.css` sheets so it wins the cascade. The existing `gpu-rentals` view needs no new stylesheet — it reuses `card`, `grid-fields`, `dialog-actions`, `field` (`panel/view-gpu-rentals.js:116-141`).

### Files a new view should import / add to index.html

1. `index.html`: add one `<li><a href="#/<route>" data-route="<route>"><label></a></li>` inside `#nav-list` (grouped under a `<li class="nav-label">` section heading), e.g. `index.html:54-57`.
2. `app.js`: import the view's `render*` and add a `{ id, re, title, navLabel, navTitle, render }` entry to `ROUTES` (`app.js:244-255`).
3. `panel/view-<route>.js`: export `async function render(mount, ctx)`.
4. CSS: none by default; add `lab-<route>.css` + `<link>` only if view-specific layout is needed.
5. `panel/test/<route>.test.mjs`: Node test (section 5).

---

## 3. View contract

### Minimal skeleton

```js
// panel/view-<name>.js — one screen, one job.
import { api } from './api.js';
import { h, emptyState, errorBlock, chip, field, toggle } from './dom.js';
import { extractList, formatUptime } from './format.js';

export async function renderGpuRentals(mount, ctx) {
  mount.replaceChildren(h('div', { class: 'card' }, 'Loading…'));

  try {
    const data = await api.get('/api/gpu-rentals');   // bare success payload
    render(data);
  } catch (err) {
    mount.replaceChildren(errorBlock(err));           // red block, full details
  }

  function render(data) {
    const root = h('div', { class: 'stack' },
      h('h2', {}, 'GPU rentals'),
      // ... built with h() only; server strings as text nodes
    );
    mount.replaceChildren(root);
  }
}
```

### What `ctx` contains

Set in `app.js:152-187` and documented at `app.js:4-6` ("views live in ./view-*.js, each exporting `render(mount, ctx, param)`. ctx carries shared services ... so views stay free of cross-imports"):

| Member | Meaning |
|---|---|
| `state` | Shared panel state: `{session, health, ports, providers, restarting}` (`app.js:34-40`) |
| `leaveGuard` | `null` or `async () => boolean`; set by a view with unsaved work; read by the hash router, `navigate()`, click interception, and the global `beforeunload` (`app.js:154,164,348,473,485`) |
| `toast` | `createToasts` result: `.success/.error/.warning/.info` (`app.js:42`, `toast.js:42-47`) |
| `confirm` | `(opts) => confirmDialog(dialogRoot, opts)` — Promise-based destroy/confirm (`dialog.js:88`) |
| `prompt` | `(opts) => promptDialog(dialogRoot, opts)` — text-input dialog (`dialog.js:120`) |
| `secretKey` | `(opts) => secretKeyDialog(dialogRoot, opts)` — one-time plaintext reveal (`dialog.js:178`) |
| `navigate` | `(hash)` — programmatic navigation that asks the guard before changing the URL (`app.js:162-177`) |
| `loadProviders` | `(force)` — cached, in-flight-deduped `GET /api/providers` (`app.js:46-56`) |
| `refreshPorts` / `refreshHealth` | Refetch `/api/ports` / `/api/health` and update chrome (`app.js:86-93,68-84`) |
| `restartFlow` | Confirm + `POST /api/restart`, poll `/api/health` up to 30s (`app.js:105-150`) |
| `onHealth` | `(cb) => {...}` — subscribe to health poll; immediate replay; returns unsubscribe (`app.js:182-186`) |

`param` is the third argument: the decoded hash capture from `ROUTES[i].re` (`app.js:287-293`), e.g. `#/combos/name`.

### Registration in app.js's route table

`app.js:244-255`:
```js
const ROUTES = [
  { id: 'overview', re: /^#\/overview$/, title: 'Overview', navLabel: 'Overview', navTitle: '…', render: renderOverview },
  …
  { id: 'gpu-rentals', re: /^#\/gpu-rentals$/, title: 'GPU rentals', navLabel: 'GPU rentals', navTitle: '…', render: renderGpuRentals },
];
```
Add the import at the top of `app.js` (alphabetical block at `app.js:17-28`) and the route entry in `ROUTES`. The regex must match the whole hash; use capture group `(?:/(.+))?` only if the view takes a parameter (`app.js:247,253`).

### Nav entry in index.html

`index.html:43-58` — inside `<ul id="nav-list">`:
```html
<li class="nav-label" aria-hidden="true">System</li>
<li><a href="#/gpu-rentals" data-route="gpu-rentals">GPU rentals</a></li>
```
The `data-route` value must equal a `ROUTES[i].id` so `renderRoute` highlights it (`app.js:296-304` sets `aria-current="page"`).

### Hash route / params

`app.js:269-325`: `renderRoute()` finds the route by regex test on `location.hash`, falls back to Overview for unknown hashes (rewriting the URL), extracts `param = decodeURIComponent(m[1])` with a raw-segment fallback on bad escaping, sets `document.title`, updates nav `aria-current`, builds `page-head` + `route-view`, calls `route.render(view, ctx, param)`, and catches late errors into `errorBlock` guarded by `routeSeq` (`app.js:317-319` — a stale view's error is discarded).

### Polling and cleanup

- Health: `ctx.onHealth(cb)`; the poller (`createPoller({tick: refreshHealth, intervalMs: 5000})`, `app.js:449`) stops while hidden (`poll.js:21-22`). `renderRoute` clears `healthSubs` on every view change (`app.js:274`) — "a view's subscription dies with the view — no manual unsubscription bookkeeping, no leaks" (`app.js:58-59`).
- Drawers: `clear(drawerRoot)` on each route change (`app.js:275`).
- A view that starts its own timer must stop it when removed — there is no global hook for that besides the route change; the cleanest pattern is to keep a local `alive = {on: true}` flag and check it (`view-overview.js:10,339,370`).

### Guarding unsaved edits on navigation

```js
ctx.leaveGuard = async () => {
  if (!dirty) return true;
  const ok = await ctx.confirm({
    title: 'Discard unsaved changes?',
    confirmLabel: 'Discard', cancelLabel: 'Keep editing', danger: true,
  });
  return ok;
};
// cleared after save/delete/revert:
ctx.leaveGuard = null;
```
`view-gpu-rentals.js:192-199` shows the remove-with-unsaved-confirm pattern using `ctx.confirm`. The global `beforeunload` at `app.js:484-488` blocks tab close/reload whenever `ctx.leaveGuard` is set, so views never register their own unload handlers.

### Showing errors via h()/errorBlock

- Endpoint failure mid-view: `mount.replaceChildren(errorBlock(err))` — red `form-errors` block with `role="alert"`; `err.message` then every `details` entry (`dom.js:337-343`).
- Form validation failure: `applyFieldErrors(scope, err)` pins leaf-matched problems next to `data-field` inputs **and** keeps everything in the list (`dom.js:406-420`); `fieldMatches` does exact or leaf-path match (`dom.js:393-398`). Then `clearFieldErrors(scope)` before each attempt (`view-keys.js:195`).
- Route-level load failure: `errorBlock(err)` + a `Retry` button calling `renderRoute()` (`app.js:320-324`).
- Fire-and-forget failures: `ctx.toast.error('…', err)` — error toasts render `details` in full (`toast.js:19-22`).

### Opening a drawer / dialog

- Drawer root: `document.getElementById('drawer-root')` (`app.js:197`); drawers are per-view UI state and are cleared on route change (`app.js:275`). CSS: `.drawer`/`.drawer-head`/`.drawer-body` (`app.css:1356-1379`).
- Dialog root: `document.getElementById('dialog-root')` (`app.js:196`).
- Use the shared primitives so the a11y contract is kept in one place (`dialog.js:236-246`):
  - `ctx.confirm({title, message, warnLines, confirmLabel, cancelLabel, danger})` → Promise<boolean>
  - `ctx.prompt({title, message, defaultValue, placeholder, …})` → Promise<string|null>
  - `ctx.secretKey({key, label})` — one-time reveal, no backdrop-dismiss (`dialog.js:178-219`)
  - `contentDialog(root, title, build(body, close))` — view-owned modal content with focus trap, Escape, focus restore

---

## 4. Backend contract

### How the conductor declares a route

Axum `Router`, route table in `crates/manarouter-conductor/src/api.rs:122-190` (`guarded_router`). Example:
```rust
.route("/api/keys", get(list_keys).post(create_key))
.route("/api/keys/{id}", put(update_key).delete(delete_key))
.route("/api/keys/{id}/rotate", post(rotate_key))
```
Handler signature:
```rust
async fn create_key(State(state): State<AppState>, JsonBody(body): JsonBody) -> ApiResult<Response>
```
`ApiResult<T> = Result<T, ApiError>` (`api.rs:345`); `ApiError` serializes as `{"error": {"code","message","details":[...]}}` (`api.rs:347-354`).

### Route ownership pattern

`api.rs:172-178`: module-owned route groups are merged in, so each feature owns its own file and the shared table is not edited per endpoint:
```rust
.merge(quota_api::routes())
.merge(models_api::routes())
.merge(activity_api::routes())
```
`quota_api.rs:91-100` is the model for a small feature API module:
```rust
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/quota", axum::routing::get(get_quota))
        .route("/api/quota/refresh", post(refresh_all))
        .route("/api/providers/{id}/accounts/{account_id}/quota/refresh", post(refresh_one))
        .route("/api/usage", axum::routing::get(get_usage))
}
```

### JSON envelope

`api.rs:1-16` (module doc): rule 1 — "Success responses are therefore bare documents (`{"providers": [...]}`…), never a success-envelope." Errors:
```json
{ "error": { "code": "validation_failed", "message": "...", "details": [ {...} ] } }
```
(`api.rs:349-351`). Panel side parses this in `api.js:22-29` (`envelope`) and `api.js:12-20` (`ApiError` with `status`, `code`, `message`, `details`).

### Auth / session requirements for /api routes

`api.rs:201-276` (`guard`): order — source check (`is_trusted_control_peer`: loopback or `100.64.0.0/10`, else 403), then Origin check for mutating methods (Origin must be loopback or exactly the Host; same host different port refused), then session cookie for `/api` paths except exempt ones (`api.rs:281-284`: GET `/api/health`, `/api/session`, POST `/api/login`, `/api/logout`). Cookie: `SESSION_COOKIE`, HttpOnly, SameSite=Strict, Secure only over HTTPS (`api.rs:659-668`), 12h TTL (`auth.rs` `SESSION_TTL`). A 401 bounces the panel to login centrally via `onUnauthorized` (`api.js:59-61`, `app.js:452-458`).

No CSRF token; protection is Origin + SameSite=Strict cookie + source-address trust (`docs/15-web-panel.md:206-211`).

### Config write mechanics — atomic, compile-before-save

`state.rs:1-24`: every mutation funnels through `AppState::commit`:
1. `Config::validate()` — invalid never reaches disk
2. `RoutingSnapshot::compile()` — rejection reported as validation failure, file untouched
3. `Config::save()` — atomic temp-file + rename
4. `RoutingSnapshot::write_to()` — atomic, after config on disk
5. `SnapshotHandle::install()` — only after bytes durable

`api.rs:1237-1251` (`commit_config_body`): `prepare_config` heals redacted secrets back (so GET→PUT round trips are loss-free), builds a `VaultTx` journal so vault writes roll back on a rejected edit ("a 400 response must never leave orphaned or overwritten secrets behind", `api.rs:18-19`), then `state.commit(prepared)`; on error, `state.tx_rollback(tx)`.

Port changes set `restart_pending`: `state.rs:229` `RestartSignal::is_pending()`; exposed via `GET /api/ports` (`api.rs:621-642`, `"restart_pending": true`); `POST /api/restart` (`api.rs:1404-1411`) calls `state.restart.request()` and returns `{"restart_pending": true}`; `main.rs:94-101` exits cleanly on the signal so the supervisor restarts. The panel shows an amber, non-dismissible banner until restart happens (`app.js:419-443`, `docs/15-web-panel.md:182-184`).

### How the relay is told about changes

Zero IPC on the request path (`LLM_INDEX.md:357-358`, `docs/01-architecture.md` §2). The conductor writes `config.json` and the compiled snapshot; the relay memory-maps the snapshot and hot-swaps it through `ArcSnap` (`LLM_INDEX.md:39-41`). The panel detects a change via the snapshot digest in `GET /api/health` (`api.rs:1051-1071`, `"snapshot": {"digest","built_at"}`); a new digest invalidates the providers cache (`app.js:79-80`). The relay picks up the new snapshot on its own schedule — the conductor never calls the relay.

### Real end-to-end handler quote

`quota_api.rs:342-365` (`refresh_one`) is a complete small handler:
```rust
async fn refresh_one(
    State(state): State<AppState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    if !config.quota.enabled { ... }
    let provider = config.providers.iter().find(...)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {provider_id}")))?;
    let account = provider.accounts.iter().find(...)
        .ok_or_else(|| ApiError::not_found(format!("no account with id {account_id}")))?;
    let result = refresh_account(&state, provider, account).await;
    Ok(Json(json!({"accounts": [result]})))
}
```
And `api.rs:371-406` shows the `ApiError` constructors (`bad`, `not_found`, `internal`, etc.) and the `From<CommitError>` mapping (validation → 400 with `details`, save failure → 500 `save_failed`, snapshot write → 500 internal).

---

## 5. Testing

### Panel tests

Runner: `node --test panel/test/*.test.mjs` (`panel/README.md:114-116`). `node:test` (`test` + `assert from 'node:assert/strict'`). 42 tests pass at the time of the audit (`docs/23-implementation-status.md:82`).

Style: pure functions and DOM-free renderers. No jsdom, no browser (`panel/README.md:108-109`). Views are imported directly and mounted against a hand-made element stub:
- `providers-refresh.test.mjs:22-82`: `makeEl()` stub with `nodeType`, `children`, `classList`, `setAttribute`/`getAttribute`, `addEventListener`, `replaceChildren`, `contains`, `querySelector`/`querySelectorAll` returning null/`[]`; `globalThis.document`, `globalThis.window`, `globalThis.localStorage` stubbed; `ctx` stub with `loadProviders`, `onHealth`, `toast`.
- `api-request.test.mjs`: stubs `globalThis.fetch` and asserts the call args (`cache: 'no-store'`, `credentials: 'same-origin'`, JSON body serialization).
- `gpu-rentals.test.mjs`: reads source files with `readFileSync(new URL('../x.js', import.meta.url))` and `assert.match` — source-level assertions when there is no DOM to render.

Best examples to copy: `panel/test/api-request.test.mjs` (API wrapper contract), `panel/test/providers-refresh.test.mjs` (mounting a real view against a controllable API + health stream), `panel/test/gpu-rentals.test.mjs` (source assertions on the files a new feature touches).

### Rust API tests

`#[tokio::test]`, `tempfile::tempdir()` so no test touches the real installation (`quota_api.rs:642-662` `state_in`). Build `AppState` with temp dirs, call handlers directly with `State(state)` and extractors, assert on `Json(body)`:
```rust
let body = usage_body(state).await;
let rows = body["rows"].as_array().expect("rows");
assert_eq!(row["estimated"], json!(true));
```
(`quota_api.rs:693-721`). Route registration is tested indirectly by the workspace suite; there is no tower-oneshot harness in-tree for handler-level HTTP tests — handlers are unit-called.

---

## 6. Checklist — add top-level view `#/gpu-rentals` backed by 3 new conductor endpoints

The repo already has a localStorage-only `view-gpu-rentals.js` + `gpu-rentals.test.mjs`; the route currently points at `renderProviderWizardRegistry` (`app.js:254`) and the nav label says "Provider setup" (`index.html:57`). This checklist converts it into a real backend-backed view.

### Panel (in order)

1. `panel/index.html:57` — change the nav item:
   ```html
   <li><a href="#/gpu-rentals" data-route="gpu-rentals">GPU rentals</a></li>
   ```
   Keep it under the `<li class="nav-label">System</li>` group or add a new group; `data-route` must match `ROUTES[i].id`.

2. `panel/view-gpu-rentals.js` — rewrite `renderGpuRentals(mount, ctx)` to fetch from the conductor instead of `localStorage`:
   - `api.get('/api/gpu-rentals')` (bare payload; `api.js:80`)
   - errors via `errorBlock(err)` / `ctx.toast.error('…', err)`; validation problems via `applyFieldErrors`
   - build only with `h()`; server strings as text nodes (`dom.js:1-8`)
   - set/clear `ctx.leaveGuard` for unsaved edits; confirm destructive actions with `ctx.confirm` (`dialog.js:88`)
   - reuse classes: `card`, `grid-fields`, `field`, `chip`, `btn`, `btn-primary`, `btn-danger`, `empty-state`, `table-scroll`, `table.tbl`

3. CSS — none required if reusing shared classes (`app.css` is already loaded). Only add `panel/lab-gpu-rentals.css` + `<link href="./lab-gpu-rentals.css">` in `index.html:12-17` after the other `lab-*.css` links if view-specific layout is unavoidable.

4. `panel/app.js:17-28` — add `import { renderGpuRentals } from './view-gpu-rentals.js';`.

5. `panel/app.js:254` — replace the route entry:
   ```js
   { id: 'gpu-rentals', re: /^#\/gpu-rentals$/, title: 'GPU rentals', navLabel: 'GPU rentals',
     navTitle: 'Plan and track GPU rental capacity.', render: renderGpuRentals },
   ```
   Hash has no parameter, so no capture group (`app.js:287-293`).

6. `panel/test/gpu-rentals.test.mjs` — add Node tests mounting the real view against a stub `ctx` (copy `providers-refresh.test.mjs:105-125` pattern) and/or source assertions (copy `gpu-rentals.test.mjs` pattern).

7. Run `node --test panel/test/*.test.mjs`.

### Backend (in order)

8. Create `crates/manarouter-conductor/src/gpu_rentals_api.rs` modeled on `quota_api.rs`:
   - module doc comment explaining ownership (`quota_api.rs:1-5`)
   - `pub fn routes() -> Router<AppState>` with 3 routes, e.g.
     ```rust
     .route("/api/gpu-rentals", get(list_gpu_rentals))
     .route("/api/gpu-rentals", post(create_gpu_rental))
     .route("/api/gpu-rentals/{id}", delete(delete_gpu_rental))
     ```
   - handlers: `async fn (State(state): State<AppState>, …) -> ApiResult<Json<Value>>`
   - success = bare JSON (`api.rs:6-9`); errors = `ApiError::bad/not_found/internal`, possibly `.with_details(vec![])`
   - read config via `state.current_config().await`; mutate via `state.commit(next).await` (`state.rs:483`) — validate → compile → save → install atomically; wrap vault writes in `VaultTx` and roll back on failure (`api.rs:1237-1251`)
   - if the feature stores no secrets, a plain handler reading/writing a JSON file or the store is fine; do **not** put plaintext credentials in responses (`api.rs:12-14`)

9. `crates/manarouter-conductor/src/api.rs:176-178` — add `.merge(gpu_rentals_api::routes())` in `guarded_router`, ahead of the `/api/{*rest}` catch-all; add `use crate::gpu_rentals_api;` at `api.rs:74`.

10. Add Rust tests in the new file: `#[tokio::test]` with `tempfile::tempdir()` and an `AppState` built like `quota_api.rs:642-662` (`state_in`); call handlers directly and assert on the JSON envelope (`{"error":{code,message,details}}` on failure, bare payload on success).

11. Run `cargo check -p manarouter-conductor` and `cargo test -p manarouter-conductor` (no git writes, no servers).

### Verification

- `node --test panel/test/*.test.mjs` — 42+ tests, no failures (`panel/README.md:123`).
- `cargo test --workspace` — workspace green (`docs/23-implementation-status.md:77`).
- Browser smoke: open the conductor URL, sign in, navigate `#/gpu-rentals`, confirm the sidebar highlights the new item (`aria-current="page"`), the page loads without an error card, and the 3 endpoints return the documented envelope.

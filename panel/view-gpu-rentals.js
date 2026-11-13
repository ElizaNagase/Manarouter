// view-gpu-rentals.js — rented GPU capacity, in two modes behind one route.
//
//   Inventory (#/gpu-rentals)         what is rented, what it has cost, what the platform
//                                     actually charges, and the bundle that deploys it.
//   Setup (#/gpu-rentals/new/<step>)  platform -> account -> model -> card -> sizing -> review.
//
// WHY both modes live in one file: they read the same endpoints, they render the same catalog
// and the same free-tier marker, and the operator crosses between them in one click. Splitting
// them would duplicate all of that. What is deliberately NOT here is the setup *logic* — step
// order, validation, probe application, sizing arithmetic and the generated app text live in
// panel/gpu-setup-model.js as pure functions. This file renders what those functions return and
// posts what the operator asks for; it does not re-implement them.
//
// WHY the deployment bundle is so prominent: Modal publishes no deployment REST API. Manarouter
// therefore cannot deploy on the operator's behalf, and the generated modal_app.py *is* the
// deployment. A view that hid that behind a button, or implied a REST call would do it, would be
// lying about what the product does. So the bundle, the command and that sentence are on the
// screen.
//
// WHY the account step comes before any model is chosen (requirement 8): an account is a platform
// credential, not a property of a model. Choosing a model first would mean holding a probe result
// that might never be deployable, and it would suggest the credential belongs to the model.
//
// NOTE ON SERVER STRINGS: platform labels, plan text, repository names, GPU ids, warning and error
// text all come from the conductor, from the platform, or from third parties. Every one of them
// enters the DOM as a text node through h(); there is deliberately no innerHTML in this file.

import { api } from './api.js';
import { h, chip, field, kv, emptyState, errorBlock, copyButton, spinner } from './dom.js';
import { formatAge, formatCompact, formatUptime, truncateMiddle } from './format.js';
import { createPoller } from './poll.js';

// ---------------------------------------------------------------------------
// Value helpers.
//
// The GPU endpoints are specified (docs/research/ARCHITECTURE-gpu-rentals.md §5) but not yet
// implemented in the backend, so no serializer exists to check key names against. Every read goes
// through pick() with the documented name first and a plausible alias after it, and every missing
// value degrades to a visible em dash rather than to "undefined" on the screen.
// ---------------------------------------------------------------------------

function pick(obj, ...keys) {
  if (!obj || typeof obj !== 'object') return null;
  for (const k of keys) {
    const v = obj[k];
    if (v !== undefined && v !== null) return v;
  }
  return null;
}

function asString(v) {
  return v === undefined || v === null ? '' : String(v);
}

function asNumber(v) {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'string' && v.trim() !== '') {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

function asBool(v) {
  if (v === true || v === false) return v;
  if (v === 'true') return true;
  if (v === 'false') return false;
  return null;
}

function listOf(v) {
  return Array.isArray(v) ? v : [];
}

function objectOf(v) {
  return v && typeof v === 'object' && !Array.isArray(v) ? v : null;
}

function dashes(v) {
  const s = asString(v).trim();
  return s || '—';
}

function errorMessage(err) {
  return asString(err?.message).trim() || 'The request failed.';
}

/**
 * Money. Sub-cent per-second prices are the whole point of the batch lane, so the digits are kept —
 * but only the significant ones: a price of $0.0011 per second reads as "$0.0011", not
 * "$0.001100". Two decimals is the floor, so a monthly plan of $0 still reads as "$0.00".
 */
function usd(v) {
  const n = asNumber(v);
  if (n === null) return '—';
  const abs = Math.abs(n);
  const digits = abs === 0 || abs >= 1 ? 2 : abs >= 0.01 ? 4 : 6;
  let text = n.toFixed(digits);
  if (digits > 2) {
    const trimmed = text.replace(/0+$/, '');
    if (trimmed.endsWith('.')) {
      // Smaller than the precision shown: "$0.000000" is honest, "$0.00" would be a lie.
    } else if (trimmed.split('.')[1].length < 2) {
      text = `${trimmed}0`;
    } else {
      text = trimmed;
    }
  }
  return `$${text}`;
}

function gibFromBytes(bytes) {
  const n = asNumber(bytes);
  if (n === null) return '—';
  return `${(n / 1024 ** 3).toFixed(1)} GiB`;
}

/** Only http(s) links are bound to an href: a catalogue string must never become a script URL. */
function safeHref(url) {
  const s = asString(url).trim();
  return /^https?:\/\//i.test(s) ? s : null;
}

function externalLink(url, label) {
  const href = safeHref(url);
  if (!href) return h('span', { class: 'mono' }, dashes(label || url));
  // The visible label is a hostname; the title carries the full URL, so a reader can tell which
  // page on that host is being cited without the cell growing to three lines.
  return h('a', { href, target: '_blank', rel: 'noopener noreferrer', title: href }, label || href);
}

// ---------------------------------------------------------------------------
// The free-tier marker (§5).
//
// `(Paid)` means one thing only: the platform's catalogue says this card is NOT available on the
// free plan (`free_plan: "unavailable"`, the H100, observed). `unknown` means exactly that —
// nobody has looked — and must never be rendered as paid: inventing an exclusion the platform
// never stated would cost the operator a card they could have used.
//
// The view keeps this rule locally rather than asking gpu-setup-model.js for it: the inventory has
// to render with the setup module absent (it is written in parallel), and the marker is a
// rendering rule about a catalogue field, not setup arithmetic.
// ---------------------------------------------------------------------------

function freePlanMarker(gpu) {
  const state = asString(pick(gpu, 'free_plan', 'freeTier', 'free_tier')).trim().toLowerCase();
  const source = asString(pick(gpu, 'source')).trim();
  const checked = asString(pick(gpu, 'checked_at', 'checkedAt')).trim();
  const provenance = [source || 'source not reported', checked ? `checked ${formatAge(checked)}` : null]
    .filter(Boolean).join(' · ');
  if (state === 'unavailable') return { paid: true, text: '(Paid)', state, provenance };
  if (state === 'available') return { paid: false, text: 'free tier', state, provenance };
  if (state === 'unknown') return { paid: false, text: 'free tier unknown', state, provenance };
  return { paid: false, text: 'free tier not reported', state, provenance };
}

/**
 * Exactly one badge is ever drawn here — `(Paid)` — because that is a limitation on what the
 * operator can rent. Every other state is plain grey text: "unknown" is an unlooked-at field, not a
 * problem, and dressing it as a chip would make the row look more substantial without saying more.
 */
function markerChip(gpu) {
  const marker = freePlanMarker(gpu);
  if (marker.paid) return chip('(Paid)', 'chip-amber', marker.provenance);
  return h('span', { class: 'hint', title: marker.provenance }, marker.text);
}

/** A card id and its label are the same name when only spacing and hyphens differ. */
function normalName(value) {
  return asString(value).toLowerCase().replace(/[\s-]+/g, '');
}

/** The catalogue's billing unit is the second; the table shows the hour because that is the unit
 *  a person budgets in. The per-second figure stays one hover away rather than in its own column. */
function secondsTitle(gpu) {
  const second = pick(gpu, 'price_usd_per_second');
  return second == null || asNumber(second) === null ? null : `${usd(second)} per second`;
}

function provenanceCell(item) {
  const source = asString(pick(item, 'source')).trim();
  const checked = asString(pick(item, 'checked_at', 'checkedAt')).trim();
  const url = pick(item, 'source_url', 'sourceUrl');
  const href = safeHref(url);
  const parts = [];
  // The source's own name is the link when there is one: "documented · 1d 17h ago · source" printed
  // the same fact twice and made a five-word column out of two.
  if (source && href) {
    parts.push(h('a', { class: 'mono gpu-prov-src', href, target: '_blank', rel: 'noopener noreferrer' }, source));
  } else if (source) {
    parts.push(h('span', { class: 'mono' }, source));
  } else if (href) {
    parts.push(h('a', { class: 'gpu-prov-src', href, target: '_blank', rel: 'noopener noreferrer' }, 'source'));
  } else {
    parts.push(h('span', { class: 'hint' }, 'source not reported'));
  }
  if (checked) parts.push(h('span', { class: 'hint', title: `checked ${checked}` }, ` · ${formatAge(checked)}`));
  const notes = asString(pick(item, 'notes')).trim();
  if (notes) parts.push(h('span', { class: 'hint' }, ' · '), h('span', { class: 'hint', title: notes }, 'note'));
  return h('div', { class: 'gpu-prov' }, ...parts);
}

// ---------------------------------------------------------------------------
// The draft the setup model works on.
//
// emptyDraft() decides the shape; the view only reads and writes through these two helpers. A key
// is written back into whichever container already owns it (draft.account.label, draft.deployment
// .gpu, ...), and only written at the top level when nothing owns it — writing a stray top-level
// `label` into a draft that is later serialised into a deployment body would hand the backend a
// field it does not accept.
// ---------------------------------------------------------------------------

const DRAFT_CONTAINERS = Object.freeze([
  'account', 'deployment', 'model', 'platform', 'card', 'rental',
]);

// `sizing` and `probe` reuse the draft's own names for computed values — sizing.gpu, probe.repo —
// because both describe the draft rather than hold part of it. They are therefore readable when
// looking a field up, and never writable: a card chosen in the form belongs on the draft, and a
// write into sizing.gpu is thrown away by the next applySizing(), which is exactly how the card
// selector came back blank after the operator picked a card.
const DRAFT_DERIVED = Object.freeze(['sizing', 'probe']);

function draftGet(draft, name) {
  if (!draft || typeof draft !== 'object') return undefined;
  if (draft[name] !== undefined && draft[name] !== null) return draft[name];
  for (const c of [...DRAFT_CONTAINERS, ...DRAFT_DERIVED]) {
    const box = objectOf(draft[c]);
    if (box && box[name] !== undefined && box[name] !== null) return box[name];
  }
  return undefined;
}

function draftSet(draft, name, value) {
  if (!draft || typeof draft !== 'object') return;
  for (const c of DRAFT_CONTAINERS) {
    const box = objectOf(draft[c]);
    if (box && Object.prototype.hasOwnProperty.call(box, name)) { box[name] = value; return; }
  }
  draft[name] = value;
}

/**
 * Requirement 5, as a rule rather than a paragraph: until the model repository field is non-empty
 * the quant selector is not populated at all. Returns the state the sizing step renders — a
 * disabled, blank selector and the sentence that explains why, or `ready: true`.
 */
export function quantGate(draft) {
  const repo = asString(draftGet(draft, 'repo')).trim();
  if (!repo) {
    return {
      ready: false,
      reason: 'The quant selector stays blank until the model repository is named on the model step. '
        + 'Nothing is fetched and nothing is guessed before then.',
    };
  }
  return { ready: true, reason: null };
}

// ---------------------------------------------------------------------------
// Setup-model plumbing. The module is loaded dynamically so the inventory mode (and its tests)
// render whether or not the setup module exists yet.
// ---------------------------------------------------------------------------

const REQUIRED_SETUP_EXPORTS = Object.freeze([
  'stepList', 'stepById', 'nextStep', 'prevStep', 'validateStep', 'emptyDraft', 'catalogIndex',
  'quantsForGpu', 'kvDtypesForGpu', 'planMarker', 'requiresHfToken', 'quantOptions', 'applyProbe',
  'applySizing', 'deploymentBundle', 'applyPayload',
]);

function missingSetupModelBlock(err, missing = null) {
  return h('section', { class: 'card gpu-missing-setup' },
    h('h2', {}, 'The GPU setup flow is not available'),
    h('p', { class: 'hint' },
      'The inventory below this route still works. The step-by-step flow is rendered from the pure '
      + 'functions in panel/gpu-setup-model.js, and that module could not be loaded, so the view is '
      + 'reporting the gap instead of drawing a form that would save nothing.'),
    errorBlock({
      message: missing
        ? `panel/gpu-setup-model.js does not export: ${missing.join(', ')}`
        : `Could not load panel/gpu-setup-model.js: ${errorMessage(err)}`,
      details: [
        { field: 'module', message: 'Expected exports: ' + REQUIRED_SETUP_EXPORTS.join(', ') },
        { field: 'module', message: 'Nothing was stubbed: a plausible form here would accept input and persist none of it.' },
      ],
    }),
    h('div', { class: 'gpu-actions' },
      h('button', {
        class: 'btn', type: 'button', onclick: () => ctxNavigate('#/gpu-rentals'),
      }, 'Back to GPU rentals')),
  );
}

// Set per render; setup navigation needs it and the module has no access to ctx at import time.
let navigateFromView = null;

function ctxNavigate(hash) {
  if (typeof navigateFromView === 'function') return navigateFromView(hash);
  return null;
}

// ---------------------------------------------------------------------------
// Shared small builders.
// ---------------------------------------------------------------------------

function dataTable(headers, rows, className = '') {
  const head = h('tr', {}, ...headers.map((hd) => h('th', {
    scope: 'col',
    class: hd.num ? 'num' : null,
  }, hd.label)));
  return h('div', { class: 'table-scroll' },
    h('table', { class: `tbl${className ? ` ${className}` : ''}` },
      h('thead', {}, head),
      h('tbody', {}, ...rows)));
}

function section(title, ...children) {
  return h('section', { class: 'card gpu-section' }, h('h2', {}, title), ...children);
}

/** A card's title row: the section name and the one action that belongs to that section. */
function cardHead(title, action = null) {
  return h('div', { class: 'card-head' }, h('h2', {}, title), action);
}

function button(label, onClick, { className = 'btn', title = null } = {}) {
  return h('button', { class: className, type: 'button', title, onclick: onClick }, label);
}

function ghostButton(label, onClick, title = null) {
  return button(label, onClick, { className: 'btn btn-ghost', title });
}

function mono(value) {
  return h('span', { class: 'mono' }, dashes(value));
}

function hint(text) {
  return h('p', { class: 'hint' }, text);
}

/**
 * An endpoint failure that names its endpoint. The status line at the top of the view names the
 * call too, but an operator who has scrolled to a section must not have to scroll back to learn
 * which request failed; the block that carries the details says it as well.
 */
function failedEndpoint(endpoint, err) {
  return h('div', {},
    h('p', { class: 'hint mono' }, `${endpoint} failed.`),
    errorBlock(err));
}

/**
 * What this platform's bundle actually is (§9/§11). Modal publishes no deployment REST API, and
 * that sentence is the one this view must never omit. Other platforms are not assumed to be the
 * same: when the catalogue records a programmatic interface the panel says the bundle is still the
 * recorded artefact, and when the catalogue is silent it says so instead of inventing an absence.
 */
function modalHonestyLine(platform) {
  const label = dashes(pick(platform, 'label') || pick(platform, 'id') || 'the platform');
  const dep = objectOf(pick(platform, 'deployment')) ?? {};
  const programmatic = asBool(pick(dep, 'programmatic'));
  if (programmatic === true) {
    const sdk = asString(pick(dep, 'sdk')).trim();
    return h('p', { class: 'gpu-honesty' },
      `${label} exposes a deployment interface${sdk ? ` (${sdk})` : ''}. This panel still records `
      + 'the deployment and hands you the generated app file and command; it is the bundle, not a '
      + 'silent API call, that you deploy and can inspect.');
  }
  if (programmatic === false) {
    return h('p', { class: 'gpu-honesty' },
      `${label} publishes no deployment REST API. Manarouter cannot deploy for you: it generates the `
      + 'app file and the exact command, and this bundle is the deployment. Run the command yourself.');
  }
  return h('p', { class: 'gpu-honesty' },
    `The catalogue does not say whether ${label} publishes a deployment REST API, so the panel does `
    + 'not claim either way. What it can show is the generated app file and the exact command; this '
    + 'bundle is what has to be deployed, and you run it yourself.');
}

function platformExplain(platform) {
  const dep = objectOf(pick(platform, 'deployment'));
  const explain = asString(pick(dep ?? {}, 'explain')).trim();
  return explain ? h('p', { class: 'hint gpu-explain' }, explain) : null;
}

function inlineNodes(value) {
  // Markdown backticks appear in the setup model's notes and hints. They become <code> elements,
  // never markup: each fragment is a text node, so a repo name or a command cannot inject HTML.
  return asString(value).split(/(`[^`]*`)/).filter(Boolean).map((part) => (
    part.startsWith('`') && part.endsWith('`') && part.length >= 2
      ? h('code', { class: 'mono' }, part.slice(1, -1))
      : document.createTextNode(part)
  ));
}

function richHint(value, className = 'hint') {
  return h('p', { class: className }, ...inlineNodes(value));
}

function noteList(title, notes, className = 'gpu-notes') {
  const items = listOf(notes).filter((n) => n !== null && n !== undefined && n !== '');
  if (!items.length) return null;
  return h('div', { class: className },
    h('h3', {}, title),
    h('ul', {}, ...items.map((n) => h('li', {}, ...inlineNodes(messageOf(n))))));
}

function messageOf(item) {
  if (typeof item === 'string' || typeof item === 'number') return String(item);
  if (item && typeof item === 'object') {
    return asString(pick(item, 'message', 'reason', 'detail', 'error', 'text')).trim() || '—';
  }
  return '—';
}

function bundlePanel(bundle, platform, { emptyNote = null, honesty = true } = {}) {
  const box = objectOf(bundle) ?? {};
  const filename = asString(pick(box, 'filename')).trim() || 'modal_app.py';
  const text = asString(pick(box, 'text', 'bundle_text')).trim();
  const dep = objectOf(pick(platform, 'deployment'));
  const command = asString(pick(box, 'command')).trim()
    || asString(pick(dep ?? {}, 'cli')).trim()
    || `modal deploy ${filename}`;
  const sdk = asString(pick(dep ?? {}, 'sdk')).trim();
  const notes = listOf(pick(box, 'notes'));
  const warnings = listOf(pick(box, 'warnings'));

  // The explanation belongs to the bundle's first appearance. The review step already says it in
  // the step hint, so it asks for the bundle without repeating itself.
  const parts = honesty ? [modalHonestyLine(platform), platformExplain(platform)] : [];
  if (text) {
    parts.push(h('div', { class: 'gpu-bundle-head' },
      h('span', { class: 'mono' }, filename),
      copyButton(text, { label: `Copy ${filename}` })));
    // A text node, never innerHTML: the app text is generated by the conductor from operator input.
    parts.push(h('pre', { class: 'gpu-pre', 'aria-label': filename }, text));
  } else if (emptyNote) {
    parts.push(hint(emptyNote));
  }
  parts.push(h('div', { class: 'gpu-bundle-head' },
    h('span', { class: 'gpu-inline-label' }, 'Exact command'),
    copyButton(command, { label: 'Copy command' })));
  // A command is a literal too: keep it in a <pre> so a copy from the screen does not inherit
  // wrapping or markup, exactly like the file above.
  parts.push(h('pre', { class: 'gpu-pre gpu-command', 'aria-label': 'deploy command' }, command));
  // Seven consecutive two-line paragraphs read as one grey wall and none of them is optional. One
  // titled list keeps every sentence, ranks it under the file and the command, and lets the eye skip
  // the whole block once it has been read.
  const noteBlock = noteList('Before you run it', notes);
  if (noteBlock) parts.push(noteBlock);
  if (warnings.length) parts.push(noteList('Warnings that affect the deployment', warnings, 'gpu-notes gpu-caveat'));
  if (sdk) parts.push(hint(`SDK alternative, when you would rather not use the CLI: ${sdk}`));
  return h('div', { class: 'gpu-bundle' }, ...parts);
}

// ---------------------------------------------------------------------------
// Inventory mode.
// ---------------------------------------------------------------------------

const USAGE_POLL_MS = 15000;

/* The lane the panel still reports on. Molab, Saturn Cloud and Cerebrium were removed from this
   screen together with their attach forms, budget and bundle: the catalogue rents on Modal, and
   Kaggle is the one host left with a lane status route. Order matters — it is the index the
   parallel fetch results are matched against. */
const LANE_PLATFORMS = Object.freeze(['kaggle']);

export async function render(mount, ctx, param = null) {
  navigateFromView = (hash) => ctx.navigate(hash);
  mount.classList?.add('gpu-rentals');
  const parts = asString(param).split('/').filter(Boolean);
  const head = (parts[0] ?? '').toLowerCase();
  if (head === 'new') {
    await renderSetup(mount, ctx, parts[1] ?? null);
    return;
  }
  // Inventory is not the setup flow: a draft kept from a previous visit must not look like it
  // belongs to the table the operator is now looking at.
  setupSession = null;
  await renderInventory(mount, ctx);
}

function normaliseAccount(raw) {
  const id = asString(pick(raw, 'id', 'account_id', 'slug')).trim();
  const token = asString(pick(raw, 'secret_ref', 'credential_ref')).trim();
  const usage = objectOf(pick(raw, 'usage'));
  return {
    id,
    label: asString(pick(raw, 'label', 'name')).trim() || id || '(unnamed account)',
    platform: asString(pick(raw, 'platform', 'platform_id')).trim(),
    plan: asString(pick(raw, 'plan', 'plan_id')).trim(),
    // The conductor reports whether the plan was actually recorded, as distinct from the plan
    // string it defaults to. Keep that distinction so an unknown plan is not read as a paid one.
    planKnown: asBool(pick(raw, 'plan_is_known', 'planKnown')),
    effectivePlan: asString(pick(raw, 'effective_plan', 'effectivePlan')).trim(),
    tokenRef: token,
    fingerprint: asString(pick(raw, 'credential_fingerprint', 'secret_fingerprint', 'fingerprint',
      'token_fingerprint', 'secret_ref_fingerprint')).trim(),
    hfFingerprint: asString(pick(raw, 'hf_token_fingerprint', 'hfFingerprint')).trim(),
    hasCredential: asBool(pick(raw, 'has_credential', 'credential_present')),
    hasHfToken: asBool(pick(raw, 'has_hf_token', 'hasHfToken')),
    maxContainers: asNumber(pick(raw, 'max_concurrent_containers', 'container_ceiling', 'max_containers')),
    note: asString(pick(raw, 'note')).trim(),
    gpuRequests: asNumber(pick(raw, 'gpu_requests', 'requests')) ?? asNumber(pick(usage, 'gpu_requests')),
    gpuSeconds: asNumber(pick(raw, 'gpu_seconds')) ?? asNumber(pick(usage, 'gpu_seconds')),
    costUsd: asNumber(pick(raw, 'cost_usd', 'cost')) ?? asNumber(pick(usage, 'cost_usd')),
    usageUnavailable: asString(pick(raw, 'usage_unavailable', 'usageUnavailable')).trim(),
    raw,
  };
}

function normaliseDeployment(raw) {
  const deployment = objectOf(pick(raw, 'deployment', 'rental')) ?? raw;
  const sizing = objectOf(pick(raw, 'sizing', 'derived_sizing')) ?? objectOf(pick(deployment, 'sizing'));
  return {
    providerId: asString(pick(raw, 'provider_id', 'providerId', 'id')).trim(),
    accountId: asString(pick(raw, 'account_id', 'account', 'accountId')).trim(),
    repo: asString(pick(deployment, 'repo', 'repository')).trim(),
    gpu: asString(pick(deployment, 'gpu', 'gpu_id')).trim(),
    gpuCount: asNumber(pick(deployment, 'gpu_count', 'gpuCount')),
    weightQuant: asString(pick(deployment, 'weight_quant', 'quant')).trim(),
    kvCacheDtype: asString(pick(deployment, 'kv_cache_dtype', 'kv_dtype')).trim(),
    mode: asString(pick(deployment, 'mode')).trim(),
    batchGpu: asString(pick(deployment, 'batch_gpu')).trim(),
    maxModelLen: asNumber(pick(deployment, 'max_model_len')),
    maxNumSeqs: asNumber(pick(deployment, 'max_num_seqs')),
    enabled: asBool(pick(raw, 'enabled')),
    sizing,
    problems: listOf(pick(raw, 'problems')).map(objectOf).filter(Boolean),
    hardwareProblems: listOf(pick(raw, 'hardware_problems', 'hardwareProblems')).map(objectOf).filter(Boolean),
    bundle: pick(raw, 'bundle', 'bundle_text'),
    bundleFilename: asString(pick(raw, 'bundle_filename')).trim(),
    command: asString(pick(raw, 'command', 'deploy_command')).trim(),
    raw,
  };
}

async function renderInventory(mount, ctx) {
  const view = {
    alive: true,
    rentals: null,
    catalog: null,
    lanes: null,
    rentalsError: null,
    catalogError: null,
    lanesError: null,
    platformId: null,
    selectedDeploymentId: null,
    updatedAt: null,
    quietFailed: false,
    laneStatuses: null,
    planBusy: false,
    planResult: null,
  };

  const countEl = h('span', { class: 'count gpu-count', 'aria-live': 'polite' }, 'GPU rentals');
  const statusEl = h('p', { class: 'hint gpu-status', role: 'status' }, 'Loading GPU rentals…');
  // The ledger total and the card count used to be five .stat boxes above the tables; the page
  // now opens the way the providers page does — one toolbar, then the list it counts.
  const usageEl = h('span', { class: 'gpu-usage' });
  const plansBox = h('div', { class: 'gpu-plans' });
  const accountsBox = h('div', { class: 'gpu-accounts' });
  const deploymentsBox = h('div', { class: 'gpu-deployments' });
  const bundleBox = h('div', { class: 'gpu-bundle-panel' });
  const catalogBox = h('div', { class: 'gpu-catalog' });
  const lanesBox = h('div', { class: 'gpu-lanes' });
  const laneCardsBox = h('div', { class: 'gpu-lane-cards' });
  const planBox = h('div', { class: 'gpu-plan' });

  // Same shape as the providers page: what the list holds on the left, the verbs on the right, and
  // one framed table underneath. The primary action lives here rather than on a section title,
  // because "New rental" is the page's action, not the table's.
  const toolbar = h('div', { class: 'toolbar gpu-toolbar' },
    h('span', { class: 'gpu-toolbar-title' }, 'Your rentals'),
    countEl,
    h('span', { class: 'spacer' }),
    ghostButton('Refresh', () => refresh(false), 'Re-read /api/gpu/rentals and /api/gpu/catalog'),
    button('New rental', () => ctx.navigate('#/gpu-rentals/new/platform'),
      { className: 'btn btn-primary', title: 'Pick a platform, account, model and card; the flow writes the app file and deploy command' }));

  // What is rented comes first, then what a rental can be attached to, then what is available.
  // The platform plumbing — plan tiers, generated bundles, lane endpoints — is reference material,
  // not the reason somebody opened this page, so it lives in one collapsed disclosure at the end.
  const advanced = h('details', { class: 'gpu-advanced' },
    h('summary', {}, 'Platform details'),
    hint('Plan tiers, the generated deploy command, and what each host allows. Nothing here is '
      + 'needed to rent a GPU.'),
    plansBox, bundleBox, lanesBox, laneCardsBox, planBox);

  // The toolbar is the head of the first frame, not a strip floating above three boxes: on the
  // providers page the counts and the verbs sit inside the same border as the table they describe.
  mount.replaceChildren(
    h('section', { class: 'card gpu-list-card' }, toolbar, statusEl, deploymentsBox),
    accountsBox, catalogBox, advanced);

  function setStatus(text) {
    statusEl.textContent = text;
    statusEl.hidden = !text;
  }

  async function refresh(quiet) {
    if (!view.alive) return;
    if (!quiet) setStatus('Reading GPU rentals…');
    const [rentals, catalog, lanes, laneStatuses] = await Promise.allSettled([
      api.get('/api/gpu/rentals'),
      api.get('/api/gpu/catalog'),
      api.get('/api/lanes'),
      Promise.allSettled(LANE_PLATFORMS.map((platform) => api.get(`/api/gpu/lanes/${platform}/status`))),
    ]);
    if (!view.alive) return;
    view.rentals = rentals.status === 'fulfilled' ? rentals.value : null;
    view.rentalsError = rentals.status === 'rejected' ? rentals.reason : null;
    view.catalog = catalog.status === 'fulfilled' ? catalog.value : null;
    view.catalogError = catalog.status === 'rejected' ? catalog.reason : null;
    view.lanes = lanes.status === 'fulfilled' ? lanes.value : null;
    view.lanesError = lanes.status === 'rejected' ? lanes.reason : null;
    // The inner `Promise.allSettled` never rejects, so the only useful signal is whether the
    // wrapper itself settled. When it did not, every lane reports the same reason rather than one
    // card silently disappearing.
    if (laneStatuses.status === 'fulfilled' && Array.isArray(laneStatuses.value)) {
      view.laneStatuses = laneStatuses.value.map((r) => (
        r.status === 'fulfilled'
          ? { ok: true, data: r.value }
          : { ok: false, error: r.reason }
      ));
    } else {
      const reason = laneStatuses.reason instanceof Error
        ? laneStatuses.reason
        : new Error('lane status requests did not settle');
      view.laneStatuses = LANE_PLATFORMS.map(() => ({ ok: false, error: reason }));
    }
    view.updatedAt = new Date();
    if (quiet) view.quietFailed = Boolean(view.rentalsError || view.catalogError);
    paint();
  }

  function platformList() {
    return listOf(pick(view.catalog, 'platforms'));
  }

  function currentPlatform() {
    const platforms = platformList();
    if (!platforms.length) return null;
    const found = platforms.find((p) => asString(pick(p, 'id')) === view.platformId);
    if (found) return found;
    view.platformId = asString(pick(platforms[0], 'id'));
    return platforms[0];
  }

  function platformById(id) {
    return platformList().find((p) => asString(pick(p, 'id')) === id) ?? null;
  }

  function planLimitsFor(platformId) {
    for (const platform of platformList()) {
      if (asString(pick(platform, 'id')) !== platformId) continue;
      return {
        platform,
        limits: listOf(pick(platform, 'plan_limits', 'planLimits')),
        maxConcurrent: objectOf(pick(platform, 'max_concurrent_containers')),
      };
    }
    return { platform: null, limits: [], maxConcurrent: null };
  }

  // -- plan limits: how the free plan actually constrains the operator ----------

  // What each host may be used for, and — where it may not — the platform's own sentence saying so.
  //
  // This is deliberately not a second copy of the catalogue: the catalogue answers "what hardware
  // exists and what it costs", this answers "what am I allowed to do with it". Two of the five hosts
  // researched for issues #10–#13 cannot be driven the way their issue asked, and the refusal on
  // this screen is quoted from the platform rather than written by this panel. A lane with no
  // refusal says "usable"; a lane with one shows the clause, because "unavailable" on its own
  // invites somebody to try to work around it.
  function paintLanes() {
    lanesBox.replaceChildren();
    const card = section('Platform lanes');
    lanesBox.append(card);
    if (view.lanesError) {
      card.append(failedEndpoint('GET /api/lanes', view.lanesError));
      return;
    }
    // The route still answers for the removed hosts; this screen does not. A row for a platform
    // the panel has no rental, status card or attach route for would only invite a question.
    const documented = new Set(['modal', ...LANE_PLATFORMS]);
    const lanes = listOf(pick(view.lanes, 'lanes'))
      .filter((lane) => documented.has(asString(pick(lane, 'id')).toLowerCase()));
    if (!lanes.length) {
      card.append(emptyState('No lane catalogue to read.',
        'GET /api/lanes returned no rows, so nothing is claimed about what a host may be used for.'));
      return;
    }
    card.append(hint(
      'What each GPU host may be used for. A refusal is the platform\'s rule quoted back, not this '
      + 'router\'s preference — and not a live quota, which only the platform counts.'));
    const rows = lanes.map((lane) => {
      const refusals = objectOf(pick(lane, 'refusals'));
      const reasons = [
        ...listOf(pick(refusals, 'synchronous')).map((r) => ({ r, traffic: 'requests' })),
        ...listOf(pick(refusals, 'batch')).map((r) => ({ r, traffic: 'batch' })),
      ];
      const seen = new Set();
      const lines = [];
      for (const item of reasons) {
        const message = asString(pick(item.r, 'message')).trim();
        const key = `${item.traffic}\u0000${message}`;
        if (!message || seen.has(key)) continue;
        seen.add(key);
        lines.push(item);
      }
      const why = lines.length
        ? h('div', { class: 'gpu-lane-why' }, ...lines.map(({ r, traffic }) => {
          const clause = asString(pick(r, 'clause')).trim();
          const source = asString(pick(r, 'source')).trim();
          const code = asString(pick(r, 'code')).trim();
          // The code goes in the title, not on the line: it is what the docs and the logs name the
          // refusal by, and it would be noise next to the sentence a human actually needs.
          return h('p', { class: 'gpu-lane-refusal' },
            h('span', { class: 'gpu-lane-traffic', title: code || null }, traffic),
            ' ',
            asString(pick(r, 'message')),
            clause ? h('q', { class: 'gpu-lane-clause' }, clause) : null,
            source ? h('span', { class: 'hint' }, ' · ', externalLink(source, 'source')) : null);
        }))
        : h('span', { class: 'gpu-lane-ok' }, 'usable');

      // One link per distinct host, so a lane that cites five pages on kaggle.com does not repeat it.
      const hosts = new Map();
      for (const url of listOf(pick(lane, 'sources'))) {
        const href = safeHref(url);
        if (!href) continue;
        let host = href;
        try { host = new URL(href).host.replace(/^www\./, ''); } catch { /* keep the url as the label */ }
        if (!hosts.has(host)) hosts.set(host, href);
      }
      const docs = hosts.size
        ? h('div', { class: 'gpu-lane-docs' }, ...[...hosts].map(([host, href]) => externalLink(href, host)))
        : h('span', { class: 'hint' }, '—');

      return h('tr', {},
        h('td', {}, h('strong', {}, dashes(pick(lane, 'display') || pick(lane, 'id')))),
        h('td', {}, chip(asString(pick(lane, 'shape')) || '—')),
        h('td', { class: 'mono' }, dashes(pick(lane, 'inbound'))),
        h('td', { class: 'mono' }, dashes(pick(lane, 'submission'))),
        h('td', {}, why),
        h('td', {}, docs));
    });
    card.append(dataTable([
      { label: 'Platform' },
      { label: 'Shape' },
      { label: 'Inbound' },
      { label: 'Submission' },
      { label: 'Requests / batch' },
      { label: 'Sources' },
    ], rows, 'tbl-lanes'));
    const headlines = lanes
      .map((lane) => ({ lane, text: asString(pick(lane, 'headline')).trim() }))
      .filter(({ text }) => text);
    if (headlines.length) {
      card.append(noteList('In one sentence', headlines.map(({ lane, text }) =>
        `${dashes(pick(lane, 'display') || pick(lane, 'id'))}: ${text}`)));
    }
  }

  /**
   * Per-platform lane cards: "Lane endpoints" — one card per platform, showing readiness, accounts,
   * warnings. Fetched independently of the inventory, so a lane that is down cannot blank the page.
   *
   * The Kaggle status route answers:
   *   { ready, accounts: [{ account, host, health, failure_count, warnings[] }] }
   */
  function readinessLabel(status) {
    // Derive a human-readable readiness from the status. Green is only when the status itself says
    // healthy or ready; otherwise we say the account count in words.
    if (status.ready === true) return { cls: 'chip-green', text: 'ready' };
    if (status.ready === false) return { cls: 'chip-neutral', text: 'not ready' };
    // No `ready` field: say the account count in words rather than invent a verdict.
    const count = (status.accounts || []).length;
    if (count === 0) return { cls: 'chip-neutral', text: 'no accounts' };
    return { cls: 'chip-neutral', text: `${count} account${count === 1 ? '' : 's'}` };
  }

  function laneAccountRow(acc) {
    const label = asString(pick(acc, 'account', 'label')).trim() || '(unnamed)';
    const host = asString(pick(acc, 'host')).trim();
    const health = asString(pick(acc, 'health')).trim();
    const probeError = asString(pick(acc, 'last_probe_error')).trim();
    const failures = asNumber(pick(acc, 'failure_count', 'failures'));

    const parts = [h('span', { class: 'mono' }, label)];
    if (host) parts.push(h('span', { class: 'hint' }, ` · ${host}`));
    if (health) parts.push(h('span', { class: 'hint' }, ` · ${health}`));
    if (failures !== null && failures > 0) parts.push(h('span', { class: 'hint mono' }, `failures: ${failures}`));
    // The lane's own measurement of the last probe, including the sentence a restored account
    // carries before it has been dialled ("restored from the vault; not probed since …").
    if (probeError) parts.push(h('span', { class: 'hint gpu-lane-probe' }, ` · ${probeError}`));
    return h('li', {}, ...parts);
  }

  /* The one lane card left. Molab, Saturn Cloud and Cerebrium had their own shapes here — a warm
     host and minutes left, a budget that is null until an operator records usage, a project and an
     app, and an attach form each. They are gone from the panel, so this renders the Kaggle shape and
     nothing it cannot be asked for. */
  function paintLaneCards() {
    laneCardsBox.replaceChildren();
    if (!view.laneStatuses) return;

    const card = section('Lane endpoint');
    card.append(hint('The lane answers on its own route, so a failure here leaves the rentals above '
      + 'untouched.'));

    const cards = [];
    for (let i = 0; i < LANE_PLATFORMS.length; i++) {
      const platform = LANE_PLATFORMS[i];
      const entry = view.laneStatuses[i];
      const statusLine = h('p', { class: 'hint mono' }, `GET /api/gpu/lanes/${platform}/status`);

      if (!entry || !entry.ok) {
        const err = entry ? entry.error : new Error('no data');
        cards.push(h('section', { class: 'card gpu-section' },
          h('h2', {}, `${platform} (endpoint unavailable)`),
          statusLine,
          failedEndpoint(`GET /api/gpu/lanes/${platform}/status`, err)));
        continue;
      }

      const status = objectOf(entry.data) ?? {};
      const accounts = listOf(pick(status, 'accounts')).map(objectOf).filter(Boolean);
      // Kaggle answers in `notes` with the platform's own wording, and in `detail` with what is
      // missing. This screen quotes the lane; it never writes a readiness sentence of its own.
      const detail = asString(pick(status, 'detail')).trim();
      const note = asString(pick(status, 'notes', 'note')).trim();
      const warnings = listOf(pick(status, 'warnings')).map(objectOf).filter(Boolean);
      const ready = readinessLabel(status);

      const parts = [];
      parts.push(h('div', { class: 'gpu-lane-status' }, chip(ready.text, ready.cls)));
      if (detail) parts.push(h('p', { class: 'hint' }, detail));
      if (note) parts.push(h('p', { class: 'hint' }, note));

      if (warnings.length) {
        const warningLines = [];
        for (const w of warnings) {
          const code = asString(pick(w, 'code')).trim();
          const msg = asString(pick(w, 'message', 'reason', 'text')).trim();
          warningLines.push(code ? `${code}: ${msg}` : msg);
        }
        parts.push(h('ul', { class: 'gpu-warnings' },
          ...warningLines.map((l) => h('li', {}, l))));
      }

      if (accounts.length) {
        parts.push(h('ul', { class: 'gpu-lane-accounts' }, ...accounts.map(laneAccountRow)));
      } else {
        parts.push(h('p', { class: 'hint' },
          'The conductor has no account attached for this platform.'));
        parts.push(h('p', { class: 'hint' },
          'This lane runs through the kaggle CLI on the host and has no attach route: install it '
          + 'and sign in before submitting a kernel.'));
      }

      const actionsRow = h('div', { class: 'gpu-actions gpu-actions-tight' },
        ghostButton('Refresh', () => refreshSingleLane(platform),
          `Re-fetch ${platform} lane status only`));
      cards.push(h('section', { class: 'card gpu-section' },
        h('h2', {}, platform),
        statusLine, ...parts,
        actionsRow));
    }
    card.append(...cards);
    laneCardsBox.append(card);
  }

  async function refreshSingleLane(platform) {
    try {
      const data = await api.get(`/api/gpu/lanes/${platform}/status`);
      // Update the slot in view.laneStatuses
      const idx = LANE_PLATFORMS.indexOf(platform);
      if (idx >= 0 && view.laneStatuses && view.laneStatuses[idx]) {
        view.laneStatuses[idx] = { ok: true, data };
      }
      paintLaneCards();
    } catch (err) {
      const idx = LANE_PLATFORMS.indexOf(platform);
      if (idx >= 0 && view.laneStatuses && view.laneStatuses[idx]) {
        view.laneStatuses[idx] = { ok: false, error: err };
      }
      paintLaneCards();
    }
  }

  /**
   * Preview a launch plan: POST /api/gpu/launch-plan with { schedule } and render the answer.
   * This is a PREVIEW ONLY — it does not start any deployment.
   */
  async function planLaunch() {
    if (view.planBusy) return;
    const select = planBox.querySelector('select');
    const schedule = select ? asString(select.value).trim() : 'sequential';
    if (!schedule) return;
    view.planBusy = true;
    view.planResult = null;
    paintPlan();
    try {
      const result = await api.post('/api/gpu/launch-plan', { schedule });
      view.planResult = objectOf(result) ?? result;
    } catch (err) {
      // Show the error in the plan box
      view.planResult = { error: err };
    } finally {
      view.planBusy = false;
      paintPlan();
    }
  }

  /**
   * Render the launch-plan preview result.
   *
   * Response shape (from gpu_api.rs launch_plan_body):
   *   schedule (string), max_live (number), waves [{cards, deployments:[indices]}],
   *   problems [{field, message}], malformed [{provider, message}], deployments[{index,provider,repo,gpu,gpu_count,account}]
   */
  function paintPlan() {
    const controls = planBox.querySelector('.gpu-plan-controls');
    if (controls) {
      const btn = controls.querySelector('button');
      if (btn) {
        btn.disabled = view.planBusy;
        btn.textContent = view.planBusy ? 'Planning…' : 'Plan launch';
      }
    }

    if (!view.planResult) return;

    // Clear previous result (keep controls)
    const existingResult = planBox.querySelector('.gpu-plan-result');
    if (existingResult) existingResult.remove();

    const box = h('div', { class: 'gpu-plan-result' });

    // Error case
    if (view.planResult.error) {
      box.append(h('p', { class: 'hint' }, 'The plan request could not be completed.'));
      box.append(errorBlock(view.planResult.error));
      planBox.append(box);
      return;
    }

    const plan = objectOf(view.planResult) ?? {};
    const schedule = asString(pick(plan, 'schedule')).trim();
    const maxLive = asNumber(pick(plan, 'max_live'));
    const note = asString(pick(plan, 'note')).trim();
    const waves = listOf(pick(plan, 'waves')).map(objectOf).filter(Boolean);
    const problems = listOf(pick(plan, 'problems')).map(objectOf).filter(Boolean);
    const malformed = listOf(pick(plan, 'malformed')).map(objectOf).filter(Boolean);
    const deployments = listOf(pick(plan, 'deployments')).map(objectOf).filter(Boolean);

    // Build a map: index → deployment
    const depMap = new Map();
    for (const d of deployments) {
      const idx = asNumber(pick(d, 'index'));
      if (idx !== null) depMap.set(idx, d);
    }

    // Summary line: schedule, max_live, note
    const summaryParts = [];
    if (schedule) summaryParts.push(`schedule: ${schedule}`);
    if (maxLive !== null) summaryParts.push(`max live: ${maxLive}`);
    if (note) summaryParts.push(note);
    if (summaryParts.length) {
      box.append(h('p', { class: 'hint' }, summaryParts.join(' · ')));
    }

    // Malformed deployments
    if (malformed.length) {
      const parts = [];
      for (const m of malformed) {
        const provider = asString(pick(m, 'provider')).trim();
        const msg = asString(pick(m, 'message')).trim();
        parts.push(h('p', { class: 'hint' }, `Deployment "${provider}": ${msg}`));
      }
      box.append(h('div', { class: 'gpu-plan-malformed' }, ...parts));
    }

    // Problems as amber warnings: "field: message"
    if (problems.length) {
      for (const p of problems) {
        const field = asString(pick(p, 'field')).trim();
        const message = asString(pick(p, 'message', 'reason', 'text')).trim();
        box.append(h('p', { class: 'hint gpu-plan-result-line' }, field ? `${field}: ${message}` : message));
      }
    }

    // Waves table
    if (waves.length === 0 && problems.length === 0 && !malformed.length) {
      box.append(h('p', { class: 'hint' }, 'No deployments are configured to launch.'));
    } else if (waves.length === 0 && !malformed.length) {
      // Problems exist but no waves — that's valid, no deployments planned
      // (already shown problems above)
    } else {
      const waveRows = waves.map((wave) => {
        const cardCount = asNumber(pick(wave, 'cards'));
        const depIndices = listOf(pick(wave, 'deployments'));
        const depNames = [];
        for (const rawIdx of depIndices) {
          const idx = asNumber(rawIdx);
          const dep = idx !== null ? depMap.get(idx) : null;
          if (dep) {
            const provider = asString(pick(dep, 'provider')).trim();
            const repo = asString(pick(dep, 'repo')).trim();
            const gpu = asString(pick(dep, 'gpu')).trim();
            const gpuCount = asNumber(pick(dep, 'gpu_count'));
            const account = asString(pick(dep, 'account')).trim();
            const name = [provider, repo, gpu].filter(Boolean).join(' / ');
            depNames.push(name || `index ${idx}`);
          } else {
            depNames.push(`index ${rawIdx}`);
          }
        }
        return h('tr', {},
          h('td', { class: 'num' }, asString(cardCount === null ? cardCount : cardCount)),
          h('td', {}, h('span', { class: 'mono' }, depNames.join(', '))));
      });
      box.append(h('h4', {}, 'Launch waves'));
      box.append(dataTable(
        [{ label: 'Cards', num: true }, { label: 'Deployments' }],
        waveRows));
    }

    planBox.append(box);
  }

  function paintPlans() {
    plansBox.replaceChildren();
    if (view.catalogError) {
      plansBox.append(section('Plan limits', failedEndpoint('GET /api/gpu/catalog', view.catalogError)));
      return;
    }
    const platforms = platformList();
    if (!platforms.length) {
      plansBox.append(section('Plan limits',
        emptyState('No platform catalogue to read.',
          'GET /api/gpu/catalog returned no platforms, so there is nothing to say about plan limits. '
          + 'The panel will not invent a price or a concurrency number.')));
      return;
    }
    const cards = [];
    for (const platform of platforms) {
      const { limits, maxConcurrent } = planLimitsFor(asString(pick(platform, 'id')));
      const platformLabel = dashes(pick(platform, 'label') || pick(platform, 'id'));
      const body = [];
      body.push(h('p', { class: 'hint gpu-plan-note' },
        `The plan boundary that matters is concurrency and included credits, not which cards exist. `
        + `Every number below is quoted from the platform's own documentation, with its source next to it.`));
      const ceiling = maxConcurrent ? asNumber(pick(maxConcurrent, 'free')) : null;
      if (!limits.length) {
        // No plan rows: the free-plan ceiling is the only documented constraint left to quote.
        if (ceiling !== null) {
          const src = asString(pick(maxConcurrent, 'source')).trim();
          body.push(h('p', { class: 'gpu-ceiling' },
            'Free-plan container concurrency: ',
            h('strong', { class: 'num' }, String(ceiling)),
            h('span', { class: 'hint' }, src ? ` · source: ${src}` : '')));
        }
        body.push(emptyState('No documented plan limits for this platform.',
          'The catalogue reported the platform without plan_limits, so the plan table is left out '
          + 'rather than filled with guesses.'));
      } else {
        const rows = limits.map((limit) => {
          const seats = asNumber(pick(limit, 'seats'));
          const concurrency = asNumber(pick(limit, 'gpu_concurrency'));
          const perMonth = asNumber(pick(limit, 'usd_per_month'));
          const credits = asNumber(pick(limit, 'included_credits_usd'));
          return h('tr', {},
            h('td', {}, h('span', { class: 'mono' }, dashes(pick(limit, 'label') || pick(limit, 'id')))),
            h('td', { class: 'num' }, perMonth === null ? '—' : `${usd(perMonth)}/mo`),
            h('td', { class: 'num' }, credits === null ? '—' : usd(credits)),
            h('td', { class: 'num' }, concurrency === null ? '—' : `${concurrency} GPU${concurrency === 1 ? '' : 's'}`),
            h('td', { class: 'num' }, seats === null ? '—' : String(seats)),
            h('td', {}, provenanceCell(limit)));
        });
        body.push(dataTable([
          { label: 'Plan' },
          { label: '$/month', num: true },
          { label: 'Included credits', num: true },
          { label: 'GPU concurrency', num: true },
          { label: 'Seats', num: true },
          { label: 'Source' },
        ], rows));
      }
      cards.push(section(`Plan limits — ${platformLabel}`, ...body));
    }
    plansBox.append(...cards);
  }

  // -- the numbers -------------------------------------------------------------

  // The conductor's ledger has one instance-wide aggregate and no per-account column, so the total
  // is stated once, beside the account table it belongs to, rather than split across the accounts.
  function paintNumbers() {
    if (view.rentalsError) {
      countEl.textContent = '—';
      usageEl.textContent = '';
      return;
    }
    const deployments = listOf(pick(view.rentals, 'deployments')).map(normaliseDeployment);
    const accounts = listOf(pick(view.rentals, 'accounts')).map(normaliseAccount);
    const usage = objectOf(pick(view.rentals, 'usage')) ?? {};
    const usageError = asString(pick(usage, 'error')).trim();
    const requests = asNumber(pick(usage, 'gpu_requests', 'requests'));
    const seconds = asNumber(pick(usage, 'gpu_seconds'));
    const cost = asNumber(pick(usage, 'cost_usd', 'cost'));
    const gpus = platformList().flatMap((p) => listOf(pick(p, 'gpus')));
    const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;

    // The card count belongs to the catalogue, which prints it on its own head; repeating it here
    // made the toolbar count a sentence nobody reads twice.
    countEl.textContent = `${count(accounts.length, 'account', 'accounts')} · `
      + `${count(deployments.length, 'rental', 'rentals')}`;

    const requestsPart = requests === null
      ? null
      : `across ${count(requests, 'request', 'requests')}`;
    usageEl.textContent = `Instance usage ${cost === null ? '—' : usd(cost)}`
      + (requestsPart ? ` ${requestsPart}` : '')
      + ` · GPU time ${seconds === null ? '—' : formatUptime(seconds)}`;
    usageEl.title = usageError || 'One ledger total for every provider on this instance.';
  }

  // -- rental accounts ---------------------------------------------------------

  function paintAccounts() {
    accountsBox.replaceChildren();
    const card = h('section', { class: 'card gpu-section' },
      cardHead('Rental accounts', h('div', { class: 'gpu-head-extras' },
        usageEl,
        ghostButton('Add rental account', () => ctx.navigate('#/gpu-rentals/new/account')))));
    if (view.rentalsError) {
      card.append(failedEndpoint('GET /api/gpu/rentals', view.rentalsError));
      accountsBox.append(card);
      return;
    }
    const accounts = listOf(pick(view.rentals, 'accounts')).map(normaliseAccount);
    card.append(hint(
      'One account is one platform credential, stored under one vault id, plus a HuggingFace token '
      + 'when a gated repository needs it. The secret is never rendered — only a fingerprint, which '
      + 'is what tells two keys apart.'));
    if (!accounts.length) {
      card.append(emptyState('No rental accounts yet.',
        'A rental account is a platform credential (Modal: a token id and its secret). Add one here; '
        + 'every deployment on that platform can share it.'));
      accountsBox.append(card);
      return;
    }
    // The conductor reports usage as one instance-wide total with no per-account filter. Four
    // columns of em dashes are noise, so they appear only when an account actually carries a
    // number; otherwise one line says where the total lives.
    const perAccount = accounts.some((a) => a.gpuRequests !== null || a.gpuSeconds !== null
      || a.costUsd !== null);
    const rows = accounts.map((account) => {
      const plan = planLimitsFor(account.platform);
      const planLimit = plan.limits.find((l) => asString(pick(l, 'id')) === account.plan);
      // An account records `free | paid | unknown`, which is a billing classification, not a plan
      // tier id. A tier row is matched only when the ids happen to agree; otherwise the honest note
      // is that no tier was recorded, not that the catalogue failed to report one.
      const planNote = planLimit
        ? `${asNumber(pick(planLimit, 'gpu_concurrency')) ?? '—'} GPU concurrency · source: ${dashes(pick(planLimit, 'source'))}`
        : plan.limits.length
          ? 'plan tier not recorded; see Platform details'
          : 'plan limits not reported for this platform';
      const credential = [];
      if (account.fingerprint) {
        credential.push(h('span', { class: 'mono', title: 'Credential fingerprint (never the value)' },
          truncateMiddle(account.fingerprint, 10, 6)));
      } else if (account.hasCredential === true) {
        credential.push(h('span', { class: 'hint' }, 'credential stored'));
      } else if (account.tokenRef) {
        credential.push(h('span', { class: 'mono' }, account.tokenRef));
      } else {
        credential.push(h('span', { class: 'hint' }, 'no credential fingerprint reported'));
      }
      const cells = [
        h('td', {},
          h('div', {}, account.label),
          account.id && account.id !== account.label ? h('span', { class: 'hint mono' }, account.id) : null),
        h('td', {}, mono(account.platform)),
        h('td', {}, h('span', {}, dashes(account.plan)), h('span', { class: 'hint', title: planNote }, ` · ${planNote}`)),
        h('td', {}, ...credential),
        h('td', { class: 'num' },
          account.maxContainers === null ? '—' : String(account.maxContainers)),
      ];
      if (perAccount) {
        cells.push(
          h('td', { class: 'num' }, account.gpuRequests === null ? '—' : String(account.gpuRequests)),
          h('td', { class: 'num' }, account.gpuSeconds === null ? '—' : formatUptime(account.gpuSeconds)),
          h('td', { class: 'num' }, account.costUsd === null ? '—' : usd(account.costUsd)));
      }
      return h('tr', {}, ...cells);
    });
    card.append(dataTable([
      { label: 'Account' },
      { label: 'Platform' },
      { label: 'Plan' },
      { label: 'Credential' },
      { label: 'Container ceiling', num: true },
      ...(perAccount ? [
        { label: 'GPU requests', num: true },
        { label: 'GPU time', num: true },
        { label: 'Cost', num: true },
      ] : []),
    ], rows));
    if (!perAccount) {
      card.append(hint('Usage is one ledger total for every provider on this instance, so it is not '
        + 'divided between accounts: the reading beside the title above is the whole of it.'));
    }
    accountsBox.append(card);
  }

  // -- deployments -------------------------------------------------------------

  function selectedDeployment(deployments) {
    if (!deployments.length) return null;
    const found = deployments.find((d) => d.providerId === view.selectedDeploymentId);
    if (found) return found;
    view.selectedDeploymentId = deployments[0].providerId;
    return deployments[0];
  }

  function paintDeployments() {
    deploymentsBox.replaceChildren();
    // No title: the toolbar that carries "Your rentals" sits inside this same frame.
    const card = h('div', { class: 'gpu-section' });
    if (view.rentalsError) {
      card.append(h('div', { class: 'gpu-list-pad' }, failedEndpoint('GET /api/gpu/rentals', view.rentalsError)));
      deploymentsBox.append(card);
      return;
    }
    const deployments = listOf(pick(view.rentals, 'deployments')).map(normaliseDeployment);
    if (!deployments.length) {
      card.append(h('div', { class: 'gpu-list-pad' }, emptyState('No GPU rentals yet.',
        'A rental is a GPU on a host, configured as a provider so the relay can route to it. Pick '
        + 'the platform, the account, the model and the card, and the panel writes the app file and '
        + 'the command that deploys it.')));
      deploymentsBox.append(card);
      return;
    }
    const problemText = (entry) => {
      const field = asString(pick(entry, 'field', 'gpu')).trim();
      const message = asString(pick(entry, 'message', 'reason')).trim() || 'problem not described';
      return field ? `${field}: ${message}` : message;
    };
    const rows = deployments.map((dep) => {
      const sizing = dep.sizing ?? {};
      const confidence = asString(pick(sizing, 'confidence')).toLowerCase();
      const sessions = asNumber(pick(sizing, 'simultaneous_sessions_at_max_len'));
      const poolTokens = asNumber(pick(sizing, 'kv_pool_tokens'));
      const isSelected = dep.providerId === view.selectedDeploymentId;
      const row = h('tr', {
        class: `row-click${isSelected ? ' row-selected' : ''}`,
        tabindex: '0',
        'data-deployment-id': dep.providerId,
        title: 'Show this deployment\'s bundle',
        onclick: () => select(dep.providerId),
        onkeydown: (ev) => {
          if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); select(dep.providerId); }
        },
      },
      h('td', {}, h('span', { class: 'mono' }, dashes(dep.providerId))),
      h('td', {}, mono(dep.accountId)),
      h('td', {}, h('span', { class: 'mono' }, dashes(dep.repo)),
        dep.maxModelLen === null ? null : h('span', { class: 'hint' }, ` · ${formatCompact(dep.maxModelLen)} ctx`)),
      h('td', {},
        h('span', { class: 'mono' }, dashes(dep.gpu)),
        dep.gpuCount && dep.gpuCount !== 1 ? h('span', { class: 'hint' }, ` ×${dep.gpuCount}`) : null),
      h('td', {},
        chip(dashes(dep.mode), 'chip-neutral'),
        dep.mode === 'batch' && dep.batchGpu
          ? h('span', { class: 'hint mono' }, ` → ${dep.batchGpu}`)
          : null),
      h('td', {}, mono(dep.weightQuant)),
      h('td', {}, mono(dep.kvCacheDtype)),
      h('td', { class: 'num', title: 'Derived from the sizing estimate; the vLLM startup line is authoritative' },
        sessions === null ? '—' : `~${sessions}`,
        dep.maxNumSeqs === null ? null : h('span', { class: 'hint' }, ` of ${dep.maxNumSeqs}`)),
      h('td', { class: 'num' },
        poolTokens === null
          ? (confidence === 'unavailable' ? h('span', { class: 'hint' }, 'unavailable') : '—')
          : h('span', { class: 'mono' }, formatCompact(poolTokens))),
      h('td', {},
        dep.enabled === false
          ? chip('disabled', 'chip-neutral', 'Switched off on purpose: not a failure')
          : dep.enabled === true
            ? chip('enabled', 'chip-green', 'This provider is routable')
            : chip('state not reported', 'chip-neutral')),
      h('td', {}, (() => {
        const entries = [...dep.problems, ...dep.hardwareProblems];
        if (!entries.length) return h('span', { class: 'hint' }, 'none');
        const title = entries.map(problemText).join(' · ');
        return chip(entries.length === 1 ? '1 problem' : `${entries.length} problems`, 'chip-red', title);
      })()));

      return row;
    });
    card.append(hint('One row per rented GPU. Select a row to see the app file and the command that '
      + 'deploys it.'));
    card.append(dataTable([
      { label: 'Provider' },
      { label: 'Account' },
      { label: 'Model' },
      { label: 'Card' },
      { label: 'Mode' },
      { label: 'Quant' },
      { label: 'KV dtype' },
      { label: 'Sessions', num: true },
      { label: 'KV pool', num: true },
      { label: 'State' },
      { label: 'Problems' },
    ], rows));
    const recordErrors = listOf(pick(view.rentals, 'errors'))
      .map((entry) => (typeof entry === 'string' ? entry.trim() : messageOf(entry)))
      .filter((entry) => entry && entry !== '—');
    if (recordErrors.length) {
      card.append(h('div', { class: 'gpu-failed gpu-record-errors' },
        h('h3', {}, 'Deployment records that could not be read'),
        h('ul', {}, ...recordErrors.map((entry) => h('li', {}, entry)))));
    }
    card.append(hint('Sessions and KV pool are estimates, marked with ~. The vLLM startup line '
      + '("GPU KV cache size: N tokens") is authoritative; these are the defaults written into the '
      + 'provider and can be overridden.'));

    // Plan-launch preview control (PREVIEW ONLY: does not start anything)
    const planControls = h('div', { class: 'gpu-plan-controls' });
    const planSchedule = h('select', {
      class: 'input',
      'aria-label': 'Launch schedule mode',
      onchange: () => { paintPlan(); },
    });
    for (const opt of ['sequential', 'batched:2', 'batched:4', 'batched:8']) {
      planSchedule.append(h('option', { value: opt }, opt));
    }
    planSchedule.value = 'sequential';
    const planBtn = h('button', {
      class: 'btn btn-primary',
      type: 'button',
      disabled: view.planBusy,
      title: 'Preview: shows how the deployments would be ordered into waves; does not start anything.',
      onclick: () => planLaunch(),
    }, view.planBusy ? 'Planning…' : 'Plan launch');
    planControls.append(planSchedule, h('span', { class: 'spacer' }), planBtn);
    planBox.replaceChildren(h('p', { class: 'hint' },
      'Preview only: shows how the configured deployments would be ordered into launch waves. '
      + 'Does not start any deployment.'));
    planBox.append(planControls);

    deploymentsBox.append(card);
  }

  function select(providerId) {
    view.selectedDeploymentId = providerId;
    paintDeployments();
    paintBundle();
  }

  function paintBundle() {
    bundleBox.replaceChildren();
    if (view.rentalsError || view.catalogError) return;
    const deployments = listOf(pick(view.rentals, 'deployments')).map(normaliseDeployment);
    const dep = selectedDeployment(deployments);
    const platform = dep ? platformById(platformIdOfAccount(dep.accountId)) : currentPlatform();
    const fallbackPlatform = platform ?? currentPlatform();
    const title = dep ? `Deployment bundle — ${dep.providerId}` : 'Deployment bundle';
    if (!dep) {
      bundleBox.append(section(title,
        bundlePanel(
          { filename: 'modal_app.py', command: asString(pick(objectOf(pick(fallbackPlatform, 'deployment')) ?? {}, 'cli')).trim() },
          fallbackPlatform,
          { emptyNote: 'No deployment yet, so there is no generated app file to show. The setup flow '
            + 'writes one at the review step, and the command below is what deploys it.' })));
      return;
    }
    const bundle = {
      filename: dep.bundleFilename || 'modal_app.py',
      text: typeof dep.bundle === 'string' ? dep.bundle : asString(pick(objectOf(dep.bundle) ?? {}, 'text')),
      command: dep.command || asString(pick(objectOf(dep.bundle) ?? {}, 'command')),
      notes: listOf(pick(objectOf(dep.bundle) ?? {}, 'notes')),
    };
    bundleBox.append(section(title,
      bundlePanel(bundle, fallbackPlatform, {
        emptyNote: 'The deployment record does not carry its generated app text, so the panel shows '
          + 'the command only rather than reconstructing a file it cannot see. Re-run the setup flow '
          + 'to regenerate the bundle.',
      })));
  }

  function platformIdOfAccount(accountId) {
    const accounts = listOf(pick(view.rentals, 'accounts')).map(normaliseAccount);
    const found = accounts.find((a) => a.id === accountId);
    return found ? found.platform : null;
  }

  // -- catalogue ---------------------------------------------------------------

  function paintCatalog() {
    catalogBox.replaceChildren();
    const cardCount = h('span', { class: 'hint' });
    const card = h('section', { class: 'card gpu-section' },
      cardHead('GPU catalogue', cardCount));
    if (view.catalogError) {
      card.append(failedEndpoint('GET /api/gpu/catalog', view.catalogError));
      catalogBox.append(card);
      return;
    }
    const platforms = platformList();
    if (!platforms.length) {
      card.append(emptyState('No GPU catalogue yet.',
        'GET /api/gpu/catalog reported no platforms, so there are no cards, prices or free-plan '
        + 'facts to show. The panel leaves the table out rather than inventing a card.'));
      catalogBox.append(card);
      return;
    }
    const platform = currentPlatform();
    const select = h('select', {
      class: 'input',
      'data-field': 'platform',
      'aria-label': 'GPU platform',
      onchange: (ev) => { view.platformId = ev.target.value; paintCatalog(); },
    });
    for (const p of platforms) {
      select.append(h('option', { value: asString(pick(p, 'id')) }, dashes(pick(p, 'label') || pick(p, 'id'))));
    }
    select.value = asString(pick(platform, 'id'));
    card.append(h('div', { class: 'gpu-catalog-head' },
      h('label', { class: 'gpu-inline-label' }, 'Platform', select),
      safeHref(pick(platform, 'docs_url'))
        ? h('span', { class: 'hint' }, ' · ', externalLink(pick(platform, 'docs_url'), 'pricing documentation'))
        : null));
    card.append(hint('Prices are per card. The free-plan column is what the platform states about '
      + 'renting that card for nothing, with its source beside it.'));

    const gpus = listOf(pick(platform, 'gpus'));
    cardCount.textContent = `${gpus.length} card${gpus.length === 1 ? '' : 's'}`;
    if (!gpus.length) {
      card.append(emptyState('No cards reported for this platform.',
        'The catalogue knows the platform but not its cards, so no price or VRAM is shown.'));
      catalogBox.append(card);
      return;
    }
    const rows = gpus.map((gpu) => {
      const vramGib = asNumber(pick(gpu, 'vram_gib'));
      const vramBytes = pick(gpu, 'vram_bytes');
      // The id and the label are the same card under two spellings ("T4", "RTX PRO 6000"); print
      // the readable one, and the id only when it says something the label does not.
      const id = asString(pick(gpu, 'id')).trim();
      const label = asString(pick(gpu, 'label')).trim();
      const sameName = normalName(id) === normalName(label);
      return h('tr', {},
        h('td', {},
          sameName
            ? h('span', {}, dashes(label || id))
            : h('span', {}, dashes(label || id), h('span', { class: 'hint mono' }, ` ${id}`))),
        h('td', { class: 'num', title: vramBytes ? asString(vramBytes) + ' bytes' : null },
          vramGib === null ? gibFromBytes(vramBytes) : `${vramGib} GiB`),
        h('td', { class: 'num', title: secondsTitle(gpu) }, usd(pick(gpu, 'price_usd_per_hour'))),
        h('td', {}, markerChip(gpu)),
        h('td', {}, provenanceCell(gpu)));
    });
    card.append(dataTable([
      { label: 'Card' },
      { label: 'VRAM', num: true },
      { label: '$/hour', num: true },
      { label: 'Free plan' },
      { label: 'Source' },
    ], rows));
    catalogBox.append(card);
  }

  function paint() {
    paintNumbers();
    if (view.rentalsError || view.catalogError) {
      const which = [view.rentalsError ? 'GET /api/gpu/rentals' : null,
        view.catalogError ? 'GET /api/gpu/catalog' : null].filter(Boolean).join(' and ');
      setStatus(`${which} failed. What is shown below comes from whichever endpoint answered.`);
    } else {
      // Nothing to report. A line saying "Updated 7:06:23 PM" was the first thing under the toolbar
      // on every visit, reporting that the page had worked. The refresh button carries the same
      // answer when it is asked for, so the line exists only when an endpoint failed.
      setStatus('');
    }
    paintPlans();
    // A failure inside the disclosure must not be hidden behind a closed summary.
    if (view.catalogError || view.lanesError) advanced.open = true;
    paintAccounts();
    paintDeployments();
    paintBundle();
    paintCatalog();
    paintLanes();
    paintLaneCards();
    paintPlan();
  }

  await refresh(false);

  // Live counters. start() fires one tick immediately, which the first fetch has already covered,
  // so the first tick is skipped; after that the table is re-read on the interval.
  let firstTick = true;
  const poller = createPoller({
    intervalMs: USAGE_POLL_MS,
    tick: async () => {
      if (!view.alive) return;
      if (typeof mount.isConnected === 'boolean' && !mount.isConnected) { poller.stop(); return; }
      if (firstTick) { firstTick = false; return; }
      await refresh(true);
    },
  });
  poller.start();
}

// ---------------------------------------------------------------------------
// Setup mode.
//
// The view is a renderer, not a rulebook. panel/gpu-setup-model.js owns the step order, the
// fields each step shows, validation, the probe merge, the sizing arithmetic and the generated
// app text. This half resolves the descriptor's `optionsSource`, draws the control, and posts the
// payload the model returns. When the model says a field is disabled or invisible, the view does
// not enable it to make the form look fuller.
//
// The draft survives a step change because the router re-renders the whole route on every hash
// change: `setupSession` is module-level, and every paint reads from it rather than from the
// discarded DOM. It is still only in memory — there is no localStorage draft to go stale against
// a new catalog.
// ---------------------------------------------------------------------------

const STEP_FALLBACK_ORDER = Object.freeze(['platform', 'account', 'model', 'card', 'sizing', 'review']);

const PLAN_LABELS = Object.freeze({ free: 'Free', paid: 'Paid', unknown: 'Unknown' });

const VIEW_OWNED_READOUTS = new Set([
  'platformDeployment', 'planMarker', 'accountCredentialNote',
  'probeGated', 'probeParams', 'probeArch', 'probeQuants', 'probeWarnings',
  'gpuPrice', 'cardPlanMarker',
  'sizingKvBasis', 'sizingKvPool', 'sizingMeasured', 'sizingCalibration', 'sizingAssumptions',
  'sizingUnavailable', 'sizingWarnings',
  'bundleFilename', 'bundleCommand', 'bundleNotes', 'bundleWarnings', 'sizingDelta',
]);

/** The live draft for this tab's setup flow. Deliberately not persisted: see the header. */
let setupSession = null;

function setupPlatform(catalog, id) {
  const wanted = asString(id).trim();
  if (!wanted) return null;
  return listOf(pick(catalog, 'platforms')).map(objectOf).filter(Boolean)
    .find((p) => sameText(pick(p, 'id'), wanted)) ?? null;
}

function setupGpu(catalog, platformId, gpuId) {
  const platform = setupPlatform(catalog, platformId);
  const wanted = asString(gpuId).trim();
  if (!platform || !wanted) return null;
  return listOf(pick(platform, 'gpus')).map(objectOf).filter(Boolean)
    .find((g) => sameText(pick(g, 'id'), wanted)) ?? null;
}

function summaryRow(label, value, { title = null } = {}) {
  const text = asString(value).trim();
  return h('div', { class: 'gpu-sum-row' },
    h('span', { class: 'gpu-sum-label' }, label),
    h('span', { class: `gpu-sum-value${text ? '' : ' gpu-sum-empty'}`, title: title ?? (text || null) },
      text || 'Not chosen'));
}

/**
 * The running record of the rental being built, beside the step that is building it.
 *
 * A six-step flow that only ever shows one step leaves the operator holding the other five in their
 * head — which platform, which card, whether the model was ever named. This is the same record the
 * review step prints in full, at a glance, and it fills the empty right-hand side that the 980px
 * column left on every step. It reads the draft, never writes it.
 */
function setupAside(wizard) {
  const draft = objectOf(wizard.draft) ?? {};
  const platformId = asString(pick(draft, 'platform')).trim();
  const platform = setupPlatform(wizard.catalog, platformId);
  const gpuId = asString(pick(draft, 'gpu')).trim();
  const gpu = setupGpu(wizard.catalog, platformId, gpuId);
  const accountId = asString(pick(draft, 'accountId', 'account_id')).trim();
  const account = listOf(wizard.accounts).find((a) => sameText(a.id, accountId)) ?? null;
  const repo = asString(pick(draft, 'repo')).trim();
  const revision = asString(pick(draft, 'revision')).trim();
  const count = Math.max(1, asNumber(pick(draft, 'gpuCount')) ?? 1);
  const sizing = objectOf(wizard.sizing) ?? objectOf(pick(draft, 'sizing')) ?? {};
  const tokens = asNumber(pick(sizing, 'max_model_len')) ?? asNumber(pick(draft, 'maxModelLen'));

  const gpuLabel = gpu
    ? asString(pick(gpu, 'label', 'name')) || gpuId
    : gpuId || '';
  const rows = [
    summaryRow('Platform', platform ? (asString(pick(platform, 'label')) || platformId) : platformId),
    summaryRow('Account', account ? account.label : accountId || ''),
    summaryRow('Model', repo ? `${repo}${revision ? `@${revision}` : ''}` : ''),
    summaryRow('Card', gpuLabel ? `${gpuLabel}${count > 1 ? ` × ${count}` : ''}` : ''),
    summaryRow('Context', tokens ? `${tokens.toLocaleString("en-US")} tokens` : ''),
  ];

  const perSecond = gpu ? asNumber(pick(gpu, 'price_usd_per_second')) : null;
  const price = perSecond === null ? null : perSecond * 3600;

  return h('aside', { class: 'gpu-setup-aside' },
    h('div', { class: 'card gpu-summary' },
      h('div', { class: 'card-head' }, h('h2', {}, 'This rental')),
      ...rows,
      price === null ? null : h('div', { class: 'gpu-sum-row gpu-sum-total' },
        h('span', { class: 'gpu-sum-label' }, 'Card price'),
        h('span', { class: 'gpu-sum-value mono', title: secondsTitle(gpu) }, `${dollarsShort(price)}/hour`)),
      h('p', { class: 'hint gpu-sum-note' },
        'Billed by the platform to the account above. Nothing here starts a container.')));
}

function firstPlatformId(catalog) {
  const platforms = listOf(pick(catalog, 'platforms'));
  for (const p of platforms) {
    const id = asString(pick(objectOf(p) ?? {}, 'id')).trim();
    if (id) return id;
  }
  return '';
}

function stepIdOf(value) {
  if (typeof value === 'string') return value.trim();
  if (value && typeof value === 'object') return asString(pick(value, 'id', 'step', 'stepId')).trim();
  return '';
}

function stepIndexOf(steps, id) {
  return steps.findIndex((s) => asString(pick(s, 'id')).trim() === id);
}

function sameText(a, b) {
  return asString(a).trim() === asString(b).trim();
}

function boolOf(value, fallback = false) {
  if (typeof value === 'boolean') return value;
  if (value === 'true' || value === 1) return true;
  if (value === 'false' || value === 0) return false;
  return fallback;
}

function listOfStrings(value) {
  return listOf(value).map((v) => asString(v).trim()).filter(Boolean);
}

function dollarsShort(value) {
  const n = asNumber(value);
  if (n === null) return '—';
  if (Number.isInteger(n)) return `$${n}`;
  return `$${n.toFixed(2)}`;
}

function bytesShort(value) {
  const n = asNumber(value);
  if (n === null) return '—';
  const gib = n / 1024 ** 3;
  return `${gib >= 10 ? Math.round(gib) : gib.toFixed(1)} GiB`;
}

function minutesShort(value) {
  const n = asNumber(value);
  if (n === null) return '—';
  const total = Math.max(0, Math.round(n));
  if (total < 60) return `${total} min`;
  const hours = Math.floor(total / 60);
  const mins = total % 60;
  return mins ? `${hours} h ${mins} min` : `${hours} h`;
}

function paramsShort(value) {
  const n = asNumber(value);
  if (n === null) return '—';
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} B parameters`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} M parameters`;
  return `${n} parameters`;
}

function minutesRange(range, fallback = null) {
  const pair = listOf(range).map(asNumber).filter((n) => n !== null);
  if (pair.length >= 2) return `${minutesShort(pair[0])}–${minutesShort(pair[1])}`;
  if (pair.length === 1) return minutesShort(pair[0]);
  return fallback === null ? '—' : minutesShort(fallback);
}

function detailsFromErrors(errors) {
  const details = [];
  for (const entry of listOf(errors)) {
    if (typeof entry === 'string' || typeof entry === 'number') {
      details.push({ field: 'error', message: String(entry) });
      continue;
    }
    const box = objectOf(entry) ?? {};
    const code = asString(pick(box, 'code')).trim();
    const message = asString(pick(box, 'message', 'reason', 'detail')).trim() || '—';
    details.push({ field: code || 'error', message });
    for (const inner of listOf(pick(box, 'details', 'problems'))) {
      if (typeof inner === 'string' || typeof inner === 'number') {
        details.push({ field: code || 'detail', message: String(inner) });
      } else {
        const d = objectOf(inner) ?? {};
        details.push({
          field: asString(pick(d, 'field', 'code')).trim() || code || 'detail',
          message: asString(pick(d, 'message', 'reason', 'detail')).trim() || '—',
        });
      }
    }
  }
  return details;
}

/** One call into the setup model, with a visible gap instead of a guessed fallback. */
function callModel(wizard, name, fallback, ...args) {
  const fn = wizard?.mod?.[name];
  if (typeof fn !== 'function') {
    const message = `panel/gpu-setup-model.js does not export ${name}(). Nothing was substituted.`;
    if (!wizard.callProblems.some((p) => p.message === message)) {
      wizard.callProblems.push({ name, message });
    }
    return fallback;
  }
  try {
    return fn(...args);
  } catch (err) {
    const message = `${name}() threw: ${errorMessage(err)}`;
    if (!wizard.callProblems.some((p) => p.message === message)) {
      wizard.callProblems.push({ name, message });
    }
    return fallback;
  }
}

function modelSteps(wizard) {
  const result = callModel(wizard, 'stepList', [], wizard.draft, wizard.catalog);
  const steps = listOf(result).filter((s) => s && typeof s === 'object');
  if (steps.length) return steps;
  // A missing stepList is reported separately; this keeps the shell drawable without inventing
  // the fields or validation rules of the six known steps.
  return STEP_FALLBACK_ORDER.map((id) => ({ id, title: id[0].toUpperCase() + id.slice(1), hint: null, fields: [] }));
}

function refreshSizing(wizard) {
  if (!wizard.probe) {
    const fromDraft = objectOf(wizard.draft?.sizing);
    if (fromDraft) wizard.sizing = fromDraft;
    return;
  }
  const result = callModel(wizard, 'applySizing', null, wizard.draft, wizard.probe, wizard.catalog);
  const box = objectOf(result);
  if (!box) return;
  if (box.draft && typeof box.draft === 'object') wizard.draft = box.draft;
  if (box.sizing && typeof box.sizing === 'object') {
    wizard.sizing = box.sizing;
    draftSet(wizard.draft, 'sizing', box.sizing);
  }
  wizard.sizingWarnings = listOf(box.warnings);
}

function refreshBundle(wizard) {
  if (!wizard.draft) return;
  const result = callModel(wizard, 'deploymentBundle', null, wizard.draft, wizard.sizing, wizard.catalog);
  const box = objectOf(result);
  if (box) {
    wizard.bundle = box;
    wizard.bundleError = null;
  }
}

/* Leaving discards the whole draft, including anything typed into the account fields -- the vault
 * only receives an account when the flow is saved. The wording says that rather than implying the
 * account step survives a departure. */
const LEAVE_CONFIRM = Object.freeze({
  title: 'Discard this setup?',
  message: 'Leaving GPU rentals discards this draft. Nothing typed here is stored, and an account '
    + 'is only written to the vault when the flow is saved; an account stored earlier stays.',
  confirmLabel: 'Leave and discard',
  cancelLabel: 'Keep editing',
  danger: true,
});

function installSetupGuard(wizard, ctx) {
  if (!ctx || typeof ctx !== 'object') return;
  const guard = () => {
    if (wizard.suppressGuard || !wizard.dirty) return true;
    if (typeof ctx.confirm !== 'function') return false;
    return ctx.confirm(LEAVE_CONFIRM);
  };
  wizard.guard = guard;
  ctx.leaveGuard = guard;
}

function retractSetupGuard(wizard, ctx) {
  if (wizard?.guard && ctx?.leaveGuard === wizard.guard) ctx.leaveGuard = null;
  if (wizard) wizard.guard = null;
}

function setupGo(wizard, hash) {
  // ctx.navigate() asks leaveGuard before it changes the hash. Advancing a step is not leaving
  // the flow, so the guard is suppressed for exactly that one call and reset as soon as the
  // router has rendered (or the navigation settles).
  wizard.suppressGuard = true;
  const result = ctxNavigate(hash);
  if (result && typeof result.then === 'function') {
    result.finally(() => { wizard.suppressGuard = false; });
  } else {
    wizard.suppressGuard = false;
  }
}

function setupHref(stepId) {
  return `#/gpu-rentals/new/${encodeURIComponent(stepId)}`;
}

function setupMountBlock(err, onRetry) {
  return h('section', { class: 'card gpu-missing-setup' },
    h('h2', {}, 'The GPU catalog could not be read'),
    failedEndpoint('GET /api/gpu/catalog', err),
    h('p', { class: 'hint' },
      'The setup flow is drawn from the catalog: platforms, cards, prices and the deployment '
      + 'mechanism all come from it. Without that response the view will not invent a platform or '
      + 'a price.'),
    h('div', { class: 'gpu-actions' },
      button('Retry', onRetry, { className: 'btn btn-primary' }),
      ghostButton('Back to GPU rentals', () => ctxNavigate('#/gpu-rentals'))));
}

function refreshSetupCatalog(wizard, mount, ctx, stepParam) {
  return async () => {
    mount.replaceChildren(h('div', { class: 'toolbar' }, spinner(), h('span', { class: 'hint' }, 'Loading the GPU catalogue…')));
    try {
      const catalog = await api.get('/api/gpu/catalog');
      wizard.catalog = objectOf(catalog) ?? {};
      draftSet(wizard.draft, 'platform', asString(wizard.draft?.platform).trim() || firstPlatformId(wizard.catalog));
      wizard.callProblems = [];
      paintSetup(wizard, mount, ctx, stepParam);
    } catch (err) {
      mount.replaceChildren(setupMountBlock(err, refreshSetupCatalog(wizard, mount, ctx, stepParam)));
    }
  };
}

async function renderSetup(mount, ctx, stepParam) {
  const wizard = setupSession ?? {
    mod: {},
    catalog: {},
    accounts: [],
    rentalsError: null,
    draft: {},
    current: null,
    maxVisited: 0,
    dirty: false,
    busy: false,
    validation: null,
    probe: null,
    sizing: null,
    sizingWarnings: [],
    bundle: null,
    bundleError: null,
    probeError: null,
    accountError: null,
    accountSignature: null,
    accountResult: null,
    applied: null,
    applyError: null,
    callProblems: [],
    suppressGuard: false,
    guard: null,
    quantResult: null,
    root: null,
    mount: null,
  };
  setupSession = wizard;
  wizard.mount = mount;
  wizard.suppressGuard = false;

  mount.classList?.add('gpu-rentals');
  mount.replaceChildren(h('div', { class: 'toolbar' }, spinner(), h('span', { class: 'hint' }, 'Loading the GPU setup flow…')));

  let mod = wizard.mod;
  if (!mod || typeof mod.stepList !== 'function') {
    try {
      mod = await import('./gpu-setup-model.js');
    } catch (err) {
      mount.replaceChildren(missingSetupModelBlock(err));
      return;
    }
    wizard.mod = mod;
  }
  const missing = REQUIRED_SETUP_EXPORTS.filter((name) => typeof mod[name] !== 'function');
  if (missing.length) {
    mount.replaceChildren(missingSetupModelBlock(null, missing));
    return;
  }

  const [catalogResult, rentalsResult] = await Promise.allSettled([
    api.get('/api/gpu/catalog'),
    api.get('/api/gpu/rentals'),
  ]);

  if (catalogResult.status === 'rejected') {
    mount.replaceChildren(setupMountBlock(catalogResult.reason, refreshSetupCatalog(wizard, mount, ctx, stepParam)));
    return;
  }
  wizard.catalog = objectOf(catalogResult.value) ?? {};
  wizard.rentalsError = rentalsResult.status === 'rejected' ? rentalsResult.reason : null;
  const rentals = rentalsResult.status === 'fulfilled' ? objectOf(rentalsResult.value) : null;
  wizard.accounts = listOf(pick(rentals ?? {}, 'accounts', 'rental_accounts')).map(normaliseAccount);

  if (!wizard.draft || typeof wizard.draft !== 'object' || !Object.keys(wizard.draft).length) {
    const platformId = firstPlatformId(wizard.catalog);
    const fresh = callModel(wizard, 'emptyDraft', null, platformId);
    wizard.draft = objectOf(fresh) ?? { platform: platformId };
    wizard.current = null;
    wizard.maxVisited = 0;
    wizard.dirty = false;
  }

  wizard.probe = wizard.probe ?? objectOf(wizard.draft.probe);
  wizard.sizing = wizard.sizing ?? objectOf(wizard.draft.sizing);
  installSetupGuard(wizard, ctx);
  paintSetup(wizard, mount, ctx, stepParam);
}

function paintSetup(wizard, mount, ctx, stepParam) {
  if (wizard.mount !== mount) return; // a newer render owns the route now
  wizard.ctx = ctx;

  let steps = modelSteps(wizard);
  const ids = steps.map((s) => asString(pick(s, 'id')).trim());
  const requested = asString(stepParam).trim();
  let current = requested && ids.includes(requested) ? requested : asString(wizard.current).trim();
  if (!ids.includes(current)) current = ids[0] ?? 'platform';
  wizard.current = current;
  const currentIndex = stepIndexOf(steps, current);
  wizard.maxVisited = Math.max(wizard.maxVisited, currentIndex);

  // Sizing feeds the review bundle and the readouts the model declares; recompute the descriptors
  // afterwards so they see the values applySizing() just wrote back into the draft.
  refreshSizing(wizard);
  if (current === 'review' || wizard.applied) refreshBundle(wizard);
  steps = modelSteps(wizard);
  const step = steps[currentIndex] ?? { id: current, title: current, fields: [] };

  const root = wizard.root && wizard.root.parentNode === mount ? wizard.root : h('div', { class: 'gpu-setup' });
  wizard.root = root;
  if (root.parentNode !== mount) mount.replaceChildren(root);

  // The status line under the rail already says whether this step has unsaved edits, so the head
  // says only where the flow is. Two copies of the same sentence were the page saying everything
  // twice, which is what made it unreadable.
  const dirtyLine = wizard.dirty
    ? 'Draft in this tab; nothing is saved until you use Continue.'
    : 'No unsaved changes in this step.';

  const head = h('div', { class: 'page-head gpu-setup-head' },
    h('div', {},
      h('h1', {}, 'New GPU rental'),
      h('p', { class: 'page-sub' },
        `Step ${currentIndex + 1} of ${steps.length} — ${asString(pick(step, 'title'))}`)),
    h('div', { class: 'gpu-actions' },
      wizard.applied ? null : ghostButton('Discard setup', () => discardSetup(wizard, ctx)),
      ghostButton('Back to GPU rentals', () => leaveSetup(wizard, ctx))));

  const rail = h('ol', { class: 'gpu-steps' }, ...steps.map((s, i) => {
    const id = ids[i];
    const isCurrent = id === current;
    const visited = i <= wizard.maxVisited;
    return h('li', { class: `gpu-step${isCurrent ? ' current' : ''}` },
      h('button', {
        class: `gpu-step-link${isCurrent ? ' gpu-step-current' : ''}${visited && !isCurrent ? ' gpu-step-done' : ''}`,
        type: 'button',
        'aria-current': isCurrent ? 'step' : null,
        disabled: wizard.busy || (!visited && i > currentIndex),
        onclick: () => goToStep(wizard, id),
      },
      h('span', { class: 'gpu-step-count' }, String(i + 1).padStart(2, '0')),
      h('span', {}, asString(pick(s, 'title')))));
  }));

  const stepHint = asString(pick(step, 'hint')).trim();
  // "No unsaved changes in this step." was a sentence printed on every step that had none of them:
  // it said nothing, in the most prominent place under the rail. The line now exists only when it
  // has something to report.
  const statusText = wizard.busy ? 'Working…' : wizard.dirty ? dirtyLine : '';
  const status = statusText
    ? h('p', { class: 'hint gpu-status', role: 'status', 'aria-live': 'polite' }, statusText)
    : null;
  const body = h('div', { class: 'gpu-step-body' });

  for (const desc of listOf(pick(step, 'fields'))) {
    // A reused account already holds its credential: rendering empty password inputs would ask the
    // operator to re-enter a secret the vault has, and an empty required field would read as an
    // error. accountReusePanel carries the explanation instead.
    const name = asString(pick(desc, 'name')).trim();
    if (current === 'account' && wizard.draft?.reuseAccount === true
      && (name === 'tokenId' || name === 'tokenSecret')) continue;
    const fieldEl = fieldRow(wizard, desc);
    if (fieldEl) body.append(fieldEl);
  }

  const extras = stepPanels(wizard, current);
  for (const extra of extras) if (extra) body.append(extra);

  const errorBlockEl = validationBlock(wizard, body);
  if (stepHint) body.prepend(richHint(stepHint, 'hint gpu-step-hint'));
  if (errorBlockEl) body.prepend(errorBlockEl);
  if (wizard.callProblems.length) body.append(renderCallProblems(wizard));
  if (wizard.applied) body.append(appliedBlock(wizard, ctx));

  // The account step is the flow's one write before the end, and it has already happened by the
  // time the review step exists. A button that says "Continue" while it stores a credential is a trap.
  const storesAccount = current === 'account' && wizard.draft?.reuseAccount !== true && !wizard.accountResult;
  const forwardLabel = wizard.busy ? 'Working…'
    : current === 'review' ? 'Save rental'
      : storesAccount ? 'Save account'
        : 'Continue';
  const actionNote = current === 'review' && !wizard.applied
    ? 'Records the request in Manarouter; it does not start a container.'
    : storesAccount
      ? 'Writes this account to the vault now. The rental itself is saved at the end.'
      : null;

  const actions = h('div', { class: 'gpu-step-actions' },
    currentIndex > 0 ? ghostButton('Back', () => goPrevious(wizard)) : null,
    h('button', {
      class: 'btn btn-primary',
      type: 'button',
      disabled: wizard.busy || Boolean(wizard.applied),
      onclick: () => advance(wizard, ctx),
    }, forwardLabel),
    actionNote ? h('span', { class: 'hint' }, actionNote) : null);

  // The step's verbs belong to the step's card, not to the page: below the card they floated at the
  // page's left edge, reading as a second set of actions for the whole route.
  body.append(actions);
  if (status) body.prepend(status);
  // Rail, step, and the record being built. Narrow viewports stack them in that order.
  const grid = h('div', { class: 'gpu-setup-grid' }, rail, body, setupAside(wizard));
  root.replaceChildren(head, grid);
}

// --- controls -------------------------------------------------------------------------------

function fieldData(el, name) {
  // Both the attribute and the dataset slot: the project's applyFieldErrors reads dataset, while a
  // selector (and the test stub) reads attributes. Keeping the two in step is the view's job.
  el.setAttribute('data-field', name);
  el.dataset.field = name;
  return el;
}

function setDisabled(control, disabled) {
  control.disabled = Boolean(disabled);
  if (disabled) control.setAttribute('disabled', '');
  else control.removeAttribute('disabled');
}

function unitSuffix(unit) {
  const text = asString(unit).trim();
  return text ? h('span', { class: 'hint gpu-unit' }, ` ${text}`) : null;
}

function notesList(notes) {
  const items = listOf(notes).filter((n) => n !== null && n !== undefined && n !== '');
  if (!items.length) return null;
  return h('ul', { class: 'gpu-notes' }, ...items.map((n) => h('li', {}, ...inlineNodes(messageOf(n)))));
}

function blankSelect(reason, name) {
  const select = fieldData(h('select', { class: 'input', 'aria-disabled': 'true' }), name);
  setDisabled(select, true);
  select.value = '';
  return { select, reason };
}

function selectRow(wizard, desc) {
  const name = asString(desc.name);
  const resolution = resolveOptions(wizard, desc);
  const choices = listOf(resolution?.options);
  const current = asString(desc.value ?? draftGet(wizard.draft, name)).trim();

  if (!resolution || !choices.length) {
    const blank = blankSelect(resolution?.reason ?? 'The catalog does not provide a value for this field, so it stays blank.', name);
    const hintText = resolution?.reason
      ?? 'The catalog does not provide a value for this field, so it stays blank rather than guessed.';
    const row = field(asString(desc.label) || name, blank.select, { hint: hintText });
    row.classList.add('gpu-field-off');
    return row;
  }

  const select = fieldData(h('select', { class: 'input' }), name);
  const values = choices.map((c) => asString(c.value));
  if (!values.includes(current)) {
    select.append(h('option', { value: '' }, 'Choose…'));
  }
  for (const choice of choices) {
    select.append(h('option', {
      value: asString(choice.value),
      selected: asString(choice.value) === current ? true : null,
    }, asString(choice.label)));
  }
  select.value = values.includes(current) ? current : '';
  if (desc.required) select.setAttribute('aria-required', 'true');
  if (desc.enabled === false) setDisabled(select, true);

  select.addEventListener('change', () => {
    commitField(wizard, name, select.value);
    paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
  });

  const row = field(asString(desc.label) || name, select);
  if (desc.hint) row.append(richHint(desc.hint));
  const notes = notesList(desc.notes);
  if (notes) row.append(notes);
  return row;
}

function inputRow(wizard, desc) {
  const name = asString(desc.name);
  const raw = desc.value ?? draftGet(wizard.draft, name);
  const kind = asString(desc.kind) || 'text';
  const type = kind === 'number' ? 'number' : kind === 'password' ? 'password' : 'text';
  const attrs = {
    class: `input${type === 'text' || type === 'password' ? ' mono' : ''}`,
    type,
    value: raw === null || raw === undefined ? '' : String(raw),
    placeholder: desc.placeholder || null,
    autocomplete: 'off',
    spellcheck: 'false',
    autocapitalize: 'off',
    min: desc.min ?? null,
    max: desc.max ?? null,
    step: desc.step ?? null,
  };
  if (desc.required) attrs['aria-required'] = 'true';
  const input = fieldData(h('input', attrs), name);
  if (desc.enabled === false) setDisabled(input, true);

  const commit = () => {
    const value = type === 'number'
      ? (input.value === '' ? null : (Number.isFinite(Number(input.value)) ? Number(input.value) : input.value))
      : input.value;
    commitField(wizard, name, value);
    // The paint replaces this input, so put the caret back where the operator left it. Without
    // this every keystroke would lose focus and the field would accept one character.
    const active = typeof document !== 'undefined' && document.activeElement === input;
    const start = active && typeof input.selectionStart === 'number' ? input.selectionStart : null;
    const end = active && typeof input.selectionEnd === 'number' ? input.selectionEnd : null;
    paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
    if (!active) return;
    const next = wizard.mount?.querySelector?.(`[data-field="${name}"]`);
    if (!next || typeof next.focus !== 'function') return;
    next.focus();
    if (start !== null && typeof next.setSelectionRange === 'function') {
      try { next.setSelectionRange(start, end ?? start); } catch { /* type=number refuses a range */ }
    }
  };
  input.addEventListener('change', commit);
  input.addEventListener('input', commit);

  const row = field(asString(desc.label) || name, input);
  if (desc.unit) row.append(unitSuffix(desc.unit));
  if (desc.hint) row.append(richHint(desc.hint));
  const notes = notesList(desc.notes);
  if (notes) row.append(notes);
  return row;
}

function readoutRow(desc) {
  const value = displayValue(desc.value);
  const row = h('div', { class: 'gpu-readout' },
    h('div', { class: 'gpu-inline-label' }, asString(desc.label)),
    h('div', { class: value === '—' ? 'hint' : 'mono' }, value));
  if (desc.hint) row.append(richHint(desc.hint));
  const notes = notesList(desc.notes);
  if (notes) row.append(notes);
  return row;
}

function displayValue(value) {
  if (value === null || value === undefined || value === '') return '—';
  if (typeof value === 'boolean') return value ? 'yes' : 'no';
  if (Array.isArray(value)) {
    const parts = value.map((v) => messageOf(v)).filter((v) => v && v !== '—');
    return parts.length ? parts.join(' · ') : '—';
  }
  if (typeof value === 'object') {
    return asString(pick(value, 'message', 'text', 'reason', 'label', 'state', 'value')).trim() || '—';
  }
  return String(value);
}

function fieldRow(wizard, desc) {
  if (!desc || typeof desc !== 'object') return null;
  const name = asString(desc.name).trim();
  if (!name) return null;
  if (desc.visible === false) return null;
  if (VIEW_OWNED_READOUTS.has(name)) return null;
  if (desc.kind === 'readout') return readoutRow(desc);
  if (desc.kind === 'select') return selectRow(wizard, desc);
  return inputRow(wizard, desc);
}

function chooseOptions(wizard, desc) {
  const source = asString(desc.optionsSource).trim();
  const catalog = wizard.catalog ?? {};
  if (source === 'catalog.platforms') {
    const platforms = listOf(pick(catalog, 'platforms')).map(objectOf).filter(Boolean);
    return platforms.map((p) => ({
      value: asString(pick(p, 'id')),
      label: asString(pick(p, 'label', 'name')) || asString(pick(p, 'id')),
    }));
  }
  if (source === 'plans') {
    const ids = Array.isArray(wizard.mod?.PLAN_IDS) ? wizard.mod.PLAN_IDS : ['free', 'paid', 'unknown'];
    return ids.map((id) => ({ value: id, label: PLAN_LABELS[id] ?? id }));
  }
  if (source === 'catalog.gpus') {
    return gpuChoices(catalog, asString(draftGet(wizard.draft, 'platform')).trim());
  }
  if (source === 'quantOptions') {
    return quantChoices(wizard);
  }
  if (source === 'kvDtypesForGpu') {
    const gpuId = asString(draftGet(wizard.draft, 'gpu')).trim();
    const entries = callModel(wizard, 'kvDtypesForGpu', [], catalog, gpuId);
    return listOf(entries)
      .map(objectOf)
      .filter((entry) => entry && entry.legal !== false)
      .map((entry) => ({
        value: asString(pick(entry, 'id')),
        label: asString(pick(entry, 'label')) || asString(pick(entry, 'id')),
      }));
  }
  if (source === 'modes') {
    return [
      { value: 'interactive', label: 'Interactive — one serving container per session' },
      { value: 'batch', label: 'Batch — offline jobs, flushed when the batch pays for its boot' },
    ];
  }
  if (source === 'catalog.gpus.price_usd_per_second' || source === 'catalog.deployment'
    || source === 'planMarker' || source === 'deploymentBundle.notes' || source === 'deploymentBundle.warnings') {
    return [];
  }
  return [];
}

function resolveOptions(wizard, desc) {
  const source = asString(desc.optionsSource).trim();
  const name = asString(desc.name);
  if (source === 'quantOptions') {
    const state = quantState(wizard);
    if (!state.ready) return { options: [], reason: state.reason };
    if (!state.choices.length) return { options: [], reason: state.reason ?? 'No quant is available for this card.' };
    return { options: state.choices, reason: null };
  }
  if (source === 'kvDtypesForGpu') {
    const gpuId = asString(draftGet(wizard.draft, 'gpu')).trim();
    if (!gpuId) return { options: [], reason: 'Choose a card first: the legal KV dtypes are resolved per card.' };
    const options = chooseOptions(wizard, desc);
    if (!options.length) {
      return {
        options: [],
        reason: `The catalog records no KV cache dtype that is legal on ${gpuId}; the selector stays blank rather than offering one the card cannot run.`,
      };
    }
    return { options, reason: null };
  }
  const options = chooseOptions(wizard, desc);
  if (!options.length) {
    const reason = source
      ? `The catalog has no options for this field (${source}); it stays blank rather than guessed.`
      : 'This field has no option source in the setup model; it stays blank rather than guessed.';
    return { options: [], reason };
  }
  return { options, reason: null };
}

function gpuChoices(catalog, platformId) {
  const platforms = listOf(pick(catalog, 'platforms')).map(objectOf).filter(Boolean);
  const scoped = platformId ? platforms.filter((p) => sameText(pick(p, 'id'), platformId)) : platforms;
  const source = scoped.length ? scoped : platforms;
  const out = [];
  for (const platform of source) {
    for (const gpu of listOf(pick(platform, 'gpus')).map(objectOf).filter(Boolean)) {
      const id = asString(pick(gpu, 'id')).trim();
      if (!id) continue;
      const vram = asNumber(pick(gpu, 'vram_bytes')) ?? (asNumber(pick(gpu, 'vram_gib')) !== null
        ? asNumber(pick(gpu, 'vram_gib')) * 1024 ** 3 : null);
      const label = asString(pick(gpu, 'label', 'name')) || id;
      out.push({ value: id, label: `${label} — ${vram === null ? 'VRAM not reported' : bytesShort(vram)}` });
    }
  }
  return out;
}

function quantState(wizard) {
  const gate = quantGate(wizard.draft);
  if (!gate.ready) {
    wizard.quantResult = { available: [], unavailable: [] };
    return { ready: false, choices: [], reason: gate.reason };
  }
  if (!wizard.probe) {
    wizard.quantResult = { available: [], unavailable: [] };
    return {
      ready: false,
      choices: [],
      reason: 'The quant selector stays blank until the repository has been probed. The probe reports '
        + 'which quants the repository actually ships; guessing one would make vLLM refuse the checkpoint. '
        + 'It is one button, just below.',
    };
  }
  const gpuId = asString(draftGet(wizard.draft, 'gpu')).trim();
  if (!gpuId) {
    wizard.quantResult = { available: [], unavailable: [] };
    return { ready: false, choices: [], reason: 'Choose a card on this step first: the quant list is filtered by the card.' };
  }
  const result = objectOf(callModel(wizard, 'quantOptions', null, wizard.probe, gpuId, wizard.catalog)) ?? {};
  wizard.quantResult = {
    available: listOf(pick(result, 'available')).map(objectOf).filter(Boolean),
    unavailable: listOf(pick(result, 'unavailable')).map(objectOf).filter(Boolean),
  };
  const choices = wizard.quantResult.available.map((q) => ({
    value: asString(pick(q, 'id')),
    label: asString(pick(q, 'label')) || asString(pick(q, 'id')),
  }));
  if (!choices.length) {
    return {
      ready: false,
      choices: [],
      reason: 'The probe found no quant this card can serve, and the selector is not given a fake "none" '
        + 'option: vLLM\'s own dtype is chosen by leaving the field unset, which is a different decision.',
    };
  }
  return { ready: true, choices, reason: null };
}

function quantChoices(wizard) {
  const state = quantState(wizard);
  return state.ready ? state.choices : [];
}

function commitField(wizard, name, value) {
  draftSet(wizard.draft, name, value);
  wizard.dirty = true;
  wizard.validation = null;
  wizard.applyError = null;
  wizard.bundle = null;

  if (name === 'platform') {
    draftSet(wizard.draft, 'gpu', '');
    draftSet(wizard.draft, 'batchGpu', null);
    wizard.probe = null;
    wizard.sizing = null;
    wizard.probeError = null;
  }
  if (name === 'repo') {
    const probeRepo = asString(pick(wizard.probe ?? {}, 'repo')).trim();
    if (probeRepo && !sameText(value, probeRepo)) {
      wizard.probe = null;
      wizard.sizing = null;
      wizard.probeError = null;
      draftSet(wizard.draft, 'probe', null);
      draftSet(wizard.draft, 'weightQuant', '');
    }
  }
  if (name === 'gpu') {
    wizard.sizing = null;
  }
  if (name === 'accountId' || name === 'plan' || name === 'tokenId' || name === 'tokenSecret') {
    // The stored result describes the account that was written. Editing an identity field makes it
    // stale, and the next Continue posts again — so the button and the panel must agree with that.
    wizard.accountSignature = null;
    wizard.accountResult = null;
  }
  if (name === 'mode' && asString(value).trim() !== 'batch') {
    draftSet(wizard.draft, 'batchGpu', null);
  }
  if (name === 'weightQuant' || name === 'kvCacheDtype' || name === 'gpuCount'
    || name === 'gpuMemoryUtilization') {
    wizard.sizing = null;
  }
  if (name === 'maxModelLen' || name === 'maxNumSeqs') {
    wizard.sizing = null;
  }
}

// --- validation -----------------------------------------------------------------------------

function normalizeValidation(result) {
  if (result === true) return { ok: true, problems: [], errors: [] };
  if (result === false) {
    return { ok: false, problems: [{ field: null, message: 'The step model rejected this step without a reason.' }], errors: [] };
  }
  const box = objectOf(result) ?? {};
  return {
    ok: Boolean(pick(box, 'ok')),
    problems: listOf(pick(box, 'problems')).map((p) => (typeof p === 'string'
      ? { field: null, message: p }
      : {
        field: asString(pick(objectOf(p) ?? {}, 'field')).trim() || null,
        message: asString(pick(objectOf(p) ?? {}, 'message', 'reason', 'detail')).trim() || '—',
      })),
    errors: listOf(pick(box, 'errors')),
  };
}

function validateCurrent(wizard) {
  const result = callModel(wizard, 'validateStep', { ok: false, problems: [], errors: [] }, wizard.draft, wizard.current, wizard.catalog);
  wizard.validation = normalizeValidation(result);
  if (!wizard.validation.ok) paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
  return Boolean(wizard.validation.ok);
}

function clearPinned(body) {
  for (const control of body.querySelectorAll('[data-field]')) {
    control.removeAttribute('aria-invalid');
    const row = control.closest('.field') ?? control.parentElement ?? control;
    for (const err of [...(row?.children ?? [])]) {
      if (err?.classList?.contains('field-error')) err.remove();
    }
  }
}

function fieldElement(body, fieldName) {
  const wanted = asString(fieldName).trim();
  if (!wanted) return null;
  const controls = [...body.querySelectorAll('[data-field]')];
  const exact = controls.find((c) => c.getAttribute('data-field') === wanted);
  if (exact) return exact;
  const leaf = wanted.split('.').pop();
  return controls.find((c) => c.getAttribute('data-field') === leaf) ?? null;
}

function validationBlock(wizard, body) {
  const validation = wizard.validation;
  if (!validation || validation.ok) return null;
  clearPinned(body);
  const parts = [];
  if (!validation.problems.length && !validation.errors.length) {
    validation.problems.push({
      field: null,
      message: `The ${wizard.current} step is not valid, but the setup model returned no problem text.`,
    });
  }
  for (const problem of validation.problems) {
    const control = fieldElement(body, problem.field);
    if (control) {
      control.setAttribute('aria-invalid', 'true');
      const row = control.closest('.field') ?? control.parentElement ?? control;
      row.after(h('p', { class: 'field-error' }, problem.field ? `${problem.field}: ${problem.message}` : problem.message));
    }
  }
  if (validation.problems.length) {
    parts.push(h('div', { class: 'form-errors', role: 'alert' },
      ...validation.problems.map((p) => h('p', {},
        p.field ? `${p.field}: ${p.message}` : p.message))));
  }
  if (validation.errors.length) {
    parts.push(errorBlock({
      message: `The ${wizard.current} step could not be validated (${validation.errors.length} error${validation.errors.length === 1 ? '' : 's'}).`,
      details: detailsFromErrors(validation.errors),
    }));
  }
  if (!parts.length) return null;
  return h('div', { class: 'gpu-validation' }, ...parts);
}

// --- step panels ----------------------------------------------------------------------------

function stepPanels(wizard, stepId) {
  if (stepId === 'platform') return [platformPanel(wizard)];
  if (stepId === 'account') {
    return [
      wizard.accountError ? failedEndpoint('POST /api/gpu/apply (create-account)', wizard.accountError) : null,
      accountReusePanel(wizard), planLimitsPanel(wizard), accountsReferencePanel(wizard), accountResultPanel(wizard),
    ];
  }
  if (stepId === 'model') return [probePanel(wizard)];
  if (stepId === 'card') {
    // The quant selector cannot be filled until the repository has been probed, and the probe
    // button is on the model step. Rather than send the operator backwards to find it, the card
    // step carries it whenever the probe is the one thing standing between them and a quant.
    const unprobed = Boolean(asString(draftGet(wizard.draft, 'repo')).trim()) && !objectOf(wizard.probe);
    return unprobed ? [cardPanel(wizard), probePanel(wizard)] : [cardPanel(wizard)];
  }
  if (stepId === 'sizing') return [sizingPanel(wizard, wizard.ctx)];
  if (stepId === 'review') return [reviewPanel(wizard)];
  return [];
}

function selectedPlatform(wizard) {
  const id = asString(draftGet(wizard.draft, 'platform')).trim();
  return listOf(pick(wizard.catalog, 'platforms')).map(objectOf).filter(Boolean)
    .find((p) => sameText(pick(p, 'id'), id)) ?? null;
}

function platformPanel(wizard) {
  const platform = selectedPlatform(wizard);
  if (!platform) {
    return h('div', { class: 'gpu-blank' }, hint('The catalog names no platform, so no deployment mechanism can be shown. Nothing was assumed.'));
  }
  const dep = objectOf(pick(platform, 'deployment'));
  // One sentence states the rule; the catalogue's longer explanation says the same thing again in
  // more detail, so it goes behind a summary instead of standing in front of the first choice the
  // operator has to make.
  const parts = [h('h3', {}, `${asString(pick(platform, 'label', 'id'))} deployment`), modalHonestyLine(platform)];
  if (dep) {
    const rows = [];
    const programmatic = pick(dep, 'programmatic');
    rows.push(['REST deployment API', programmatic === true ? 'yes' : programmatic === false ? 'no' : '—']);
    if (asString(pick(dep, 'cli')).trim()) rows.push(['CLI', asString(pick(dep, 'cli')).trim()]);
    if (asString(pick(dep, 'sdk')).trim()) rows.push(['SDK', asString(pick(dep, 'sdk')).trim()]);
    if (rows.length) parts.push(kv(rows));
  } else {
    parts.push(hint('The catalog does not describe how deployments are created for this platform. The bundle on the review step is still what Manarouter can produce.'));
  }
  const explain = platformExplain(platform);
  if (explain) {
    parts.push(h('details', { class: 'gpu-advanced' },
      h('summary', {}, 'Why the panel cannot run the deploy'), explain));
  }
  return h('div', { class: 'gpu-explain' }, ...parts);
}

function planLimitsPanel(wizard) {
  const platform = selectedPlatform(wizard);
  if (!platform) return null;
  const platformLabel = asString(pick(platform, 'label', 'id')) || 'this platform';
  const limits = listOf(pick(platform, 'plan_limits')).map(objectOf).filter(Boolean);
  if (limits.length) {
    const rows = limits.map((plan) => h('tr', {},
      h('td', {}, h('strong', {}, asString(pick(plan, 'label', 'id')) || '—'),
        h('span', { class: 'hint mono' }, ` ${asString(pick(plan, 'id'))}`)),
      h('td', { class: 'num' }, `${dollarsShort(pick(plan, 'usd_per_month'))}/month`),
      h('td', { class: 'num' }, dollarsShort(pick(plan, 'included_credits_usd'))),
      h('td', { class: 'num' }, dashes(asNumber(pick(plan, 'gpu_concurrency')))),
      h('td', { class: 'num' }, dashes(asNumber(pick(plan, 'seats')))),
      h('td', {}, provenanceCell(plan))));
    return h('div', { class: 'gpu-plans' },
      h('h3', {}, `What ${platformLabel} publishes for its plans`),
      dataTable([
        { label: 'Plan' }, { label: 'Plan price', num: true }, { label: 'Included credits', num: true },
        { label: 'GPU concurrency', num: true }, { label: 'Seats', num: true }, { label: 'Source' },
      ], rows),
      richHint('These are published limits, not enforcement by this panel. Concurrency is counted per account; the account step stores one account at a time.'));
  }

  const ceiling = objectOf(pick(platform, 'max_concurrent_containers'));
  if (ceiling) {
    const free = asNumber(pick(ceiling, 'free'));
    return h('div', { class: 'gpu-plans' },
      h('h3', {}, `What ${platformLabel} publishes for its plans`),
      kv([['Free-plan GPU concurrency', free === null ? '—' : String(free)], ['Source', asString(pick(ceiling, 'source')) || 'not reported']]),
      richHint('The catalog carries no `plan_limits` array for this platform, so only the documented fallback is shown. Plan prices, credits and seats are not invented here.'));
  }
  return h('div', { class: 'gpu-plans' },
    h('h3', {}, `What ${platformLabel} publishes for its plans`),
    hint('The catalog carries no plan limits for this platform. This view does not guess a price, a credit allowance or a concurrency ceiling.'));
}

function accountsReferencePanel(wizard) {
  const platform = asString(draftGet(wizard.draft, 'platform')).trim();
  const accounts = wizard.accounts.filter((a) => !platform || sameText(a.platform, platform));
  if (!accounts.length) {
    return h('div', { class: 'gpu-blank' },
      hint(wizard.rentalsError
        ? `GET /api/gpu/rentals could not be read, so existing accounts are unknown: ${errorMessage(wizard.rentalsError)}`
        : 'No account on this platform has been recorded yet. The account you create here is stored in the vault before any model is chosen.'));
  }
  const rows = accounts.map((a) => h('tr', {},
    h('td', {}, mono(a.id), a.label && a.label !== a.id ? h('div', { class: 'hint' }, a.label) : null),
    h('td', {}, a.plan || '—'),
    h('td', {}, a.fingerprint ? mono(a.fingerprint) : h('span', { class: 'hint' }, a.hasCredential ? 'stored' : 'not reported')),
    h('td', {}, h('button', {
      class: 'btn btn-sm btn-ghost',
      type: 'button',
      disabled: wizard.busy,
      'aria-label': `Use account ${a.id} for this deployment`,
      title: 'Bill this deployment to the credential already stored for this account.',
      onclick: () => useExistingAccount(wizard, a),
    }, 'USE'))));
  return h('div', { class: 'gpu-existing' },
    h('h3', {}, 'Accounts already recorded for this platform'),
    dataTable([{ label: 'Account' }, { label: 'Plan' }, { label: 'Credential' }, { label: '' }], rows),
    richHint('An account id is global to this installation. “USE” bills the deployment to that account’s stored credential without sending a token; it is the way a second deployment shares a workspace.'));
}

/** Point the draft at an account that is already in the vault, without re-posting its token. */
function useExistingAccount(wizard, account) {
  if (wizard.busy || !account) return;
  draftSet(wizard.draft, 'accountId', account.id);
  draftSet(wizard.draft, 'reuseAccount', true);
  draftSet(wizard.draft, 'tokenId', '');
  draftSet(wizard.draft, 'tokenSecret', '');
  wizard.accountError = null;
  wizard.accountResult = null;
  wizard.accountSignature = null;
  paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
}

/**
 * The account step while it is billing an existing account: no token is asked for, and the copy
 * says what is and is not happening. The way back to recording a fresh account is a button, not
 * a hidden state change.
 */
function accountReusePanel(wizard) {
  if (wizard.draft?.reuseAccount !== true) return null;
  const id = asString(draftGet(wizard.draft, 'accountId')).trim();
  return h('div', { class: 'gpu-existing' },
    h('h3', {}, `Billing account: ${id || 'none selected'}`),
    h('p', { class: 'hint' },
      'This deployment uses the credential already stored for that account. No token is sent and nothing in the vault is overwritten: the account was recorded before this draft existed.'),
    h('p', { class: 'hint' },
      'To bill a different workspace, record a new account with its own id and token.'),
    h('div', { class: 'gpu-actions' }, ghostButton('Create a new account instead', () => {
      draftSet(wizard.draft, 'reuseAccount', false);
      wizard.accountResult = null;
      wizard.accountSignature = null;
      paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
    })));
}

function accountResultPanel(wizard) {
  // A reused account was stored by an earlier visit; saying "stored" here would imply this draft
  // sent something. accountReusePanel already says what the deployment bills.
  if (wizard.draft?.reuseAccount === true) return null;
  if (!wizard.accountResult) return null;
  const result = objectOf(wizard.accountResult) ?? {};
  const account = objectOf(pick(result, 'account')) ?? result;
  const id = asString(pick(account, 'id', 'account_id')).trim();
  const fingerprint = asString(pick(account, 'secret_fingerprint', 'fingerprint', 'credential_fingerprint')).trim();
  return h('div', { class: 'gpu-applied' },
    h('h3', {}, 'Account stored'),
    kv([
      ['Account id', id || '—'],
      ['Credential', fingerprint ? truncateMiddle(fingerprint, 10, 6) : 'stored; no fingerprint was returned'],
    ]),
    richHint('The token itself is never rendered or read back. Changing it later means submitting the account step again.'));
}

function probePanel(wizard) {
  const repo = asString(draftGet(wizard.draft, 'repo')).trim();
  const probe = objectOf(wizard.probe);
  const busy = Boolean(wizard.busy);
  const probeButton = h('button', {
    class: 'btn',
    type: 'button',
    disabled: busy || !repo,
    title: repo ? null : 'Paste a model repository first.',
    onclick: () => runProbe(wizard, wizard.ctx),
  }, busy ? 'Probing…' : probe ? 'Probe again' : 'Probe repository');

  const parts = [h('h3', {}, 'Repository probe'),
    h('p', { class: 'hint' }, 'The probe reads the repository and its config. It reports the quants that exist and how large the KV pool can be; neither is guessed from the model name.'),
    h('div', { class: 'gpu-actions' }, probeButton)];

  if (wizard.probeError) parts.push(failedEndpoint('POST /api/gpu/probe', wizard.probeError));
  if (probe) {
    const gated = objectOf(pick(probe, 'gated'));
    const arch = objectOf(pick(probe, 'arch'));
    const params = objectOf(pick(probe, 'params'));
    const sha = asString(pick(probe, 'sha')).trim();
    const gatedState = asString(pick(gated ?? {}, 'state')).trim() || (pick(gated ?? {}, 'needs_token') === true ? 'gated' : '—');
    const gatedExplain = asString(pick(gated ?? {}, 'explain')).trim();
    const quants = listOf(pick(probe, 'quants')).map((q) => asString(pick(objectOf(q) ?? {}, 'id'))).filter(Boolean);
    const total = asNumber(pick(params ?? {}, 'total'));
    const paramsSource = asString(pick(params ?? {}, 'source')).trim();
    const agrees = pick(params ?? {}, 'agrees_with_name');
    const rows = [
      ['Repository', asString(pick(probe, 'repo')).trim() || repo],
      ['Revision', asString(pick(probe, 'revision')).trim() || '—'],
      ['Commit', sha ? truncateMiddle(sha, 10, 6) : '—'],
      ['Gated', gatedState],
      ['Parameters', total === null ? '—' : `${paramsShort(total)}${paramsSource ? ` (${paramsSource})` : ''}`],
      ['Architecture', [
        asNumber(pick(arch ?? {}, 'num_hidden_layers', 'numLayers')) !== null ? `${asNumber(pick(arch ?? {}, 'num_hidden_layers', 'numLayers'))} layers` : null,
        asNumber(pick(arch ?? {}, 'num_key_value_heads', 'numKeyValueHeads', 'num_kv_heads')) !== null ? `${asNumber(pick(arch ?? {}, 'num_key_value_heads', 'numKeyValueHeads', 'num_kv_heads'))} KV heads` : null,
        asNumber(pick(arch ?? {}, 'head_dim')) !== null ? `head dim ${asNumber(pick(arch ?? {}, 'head_dim'))}` : null,
      ].filter(Boolean).join(' · ') || '—'],
      ['Max context', asNumber(pick(arch ?? {}, 'max_position_embeddings')) === null ? '—'
        : String(asNumber(pick(arch ?? {}, 'max_position_embeddings')))],
      ['Quants in the repository', quants.length ? quants.join(' · ') : 'none reported'],
    ];
    parts.push(kv(rows));
    if (gatedExplain) parts.push(richHint(gatedExplain));
    if (agrees === false) parts.push(richHint('The parameter count read from the config does not agree with the repository name. Treat the name as a label, not a fact.'));
    const warnings = listOfStrings(pick(probe, 'warnings'));
    if (warnings.length) parts.push(noteList('Probe warnings', warnings));
    const probeErrors = listOf(pick(probe, 'errors'));
    if (probeErrors.length) {
      parts.push(errorBlock({ message: 'The probe reported errors.', details: detailsFromErrors(probeErrors) }));
    }
  } else if (repo) {
    parts.push(h('p', { class: 'hint' }, 'No probe result yet. The quant and KV selectors stay blank until this repository has been probed.'));
  } else {
    parts.push(h('p', { class: 'hint' }, 'The probe needs a repository in `owner/name` form. Paste it above, then run the probe.'));
  }
  return h('div', { class: 'gpu-probe' }, ...parts);
}

function cardPanel(wizard) {
  const gpuId = asString(draftGet(wizard.draft, 'gpu')).trim();
  const platform = selectedPlatform(wizard);
  const gpu = gpuId && platform
    ? listOf(pick(platform, 'gpus')).map(objectOf).filter(Boolean).find((g) => sameText(pick(g, 'id'), gpuId)) ?? null
    : null;
  const parts = [];
  if (gpu) {
    const vram = asNumber(pick(gpu, 'vram_bytes')) ?? (asNumber(pick(gpu, 'vram_gib')) !== null
      ? asNumber(pick(gpu, 'vram_gib')) * 1024 ** 3 : null);
    const perHour = asNumber(pick(gpu, 'price_usd_per_hour', 'usd_per_hour'));
    const perSecond = asNumber(pick(gpu, 'price_usd_per_second', 'usd_per_second'));
    parts.push(h('h3', {}, `${asString(pick(gpu, 'label', 'id'))} from the catalog`));
    parts.push(kv([
      ['VRAM', vram === null ? '—' : bytesShort(vram)],
      ['Price', `${perHour === null ? '—' : `${usd(perHour)}/hour`} · ${perSecond === null ? 'no per-second rate reported' : `${usd(perSecond)}/second`}`],
      ['Free plan', freePlanMarker(gpu).text],
    ]));
    parts.push(provenanceCell(gpu));
    const marker = objectOf(callModel(wizard, 'planMarker', null, wizard.catalog, asString(pick(platform, 'id')),
      { plan: asString(draftGet(wizard.draft, 'plan')).trim(), gpu: gpuId }));
    if (marker && marker.paid) {
      parts.push(h('p', { class: 'gpu-plan-marker' }, chip('(Paid)', 'chip-amber'), h('span', { class: 'hint' }, ` ${asString(marker.provenance)}`)));
    } else if (marker && asString(marker.provenance).trim()) {
      parts.push(h('p', { class: 'hint' }, asString(marker.provenance)));
    }
  } else if (gpuId) {
    parts.push(h('h3', {}, `${gpuId} is not in the catalog`));
    parts.push(h('p', { class: 'hint' }, 'The catalog has no VRAM or price for this card. Nothing is assumed: Modal will reject an id it does not publish, and the bundle says so too.'));
  } else {
    parts.push(h('p', { class: 'hint' }, 'Choose a card to see its published VRAM, its hourly and per-second rate, and whether it is available on the free plan.'));
  }

  const unavailable = listOf(wizard.quantResult?.unavailable).map(objectOf).filter(Boolean);
  for (const item of unavailable) {
    const plan = objectOf(pick(item, 'requantize'));
    if (plan) {
      parts.push(requantizePanel(plan, gpuId, asString(pick(item, 'id'))));
    } else {
      parts.push(h('p', { class: 'hint' },
        `${asString(pick(item, 'id'))} is unavailable on this card: ${asString(pick(item, 'reason'), 'no reason was reported')}.`));
    }
  }
  if (!parts.length) return null;
  return h('div', { class: 'gpu-card-facts' }, ...parts);
}

function requantizePanel(plan, gpuId, requestedId) {
  const requested = asString(pick(plan, 'requested')).trim() || requestedId || 'the requested quant';
  const reason = asString(pick(plan, 'unavailable_reason', 'reason')).trim();
  const tool = asString(pick(plan, 'tool')).trim();
  const minutes = asNumber(pick(plan, 'estimated_minutes'));
  const range = minutesRange(pick(plan, 'estimated_minutes_range'), minutes);
  const vram = asNumber(pick(plan, 'requires_vram_bytes'));
  const extra = asNumber(pick(plan, 'requires_extra_vram_bytes'));
  const hostRam = asNumber(pick(plan, 'requires_host_ram_bytes'));
  const disk = asNumber(pick(plan, 'requires_disk_bytes'));
  const feasible = pick(plan, 'feasible_on_gpu');
  const paths = listOf(pick(plan, 'paths')).map(objectOf).filter(Boolean);

  const facts = [];
  if (tool) facts.push(['Tool', tool]);
  facts.push(['Estimated time', range]);
  if (vram !== null) facts.push(['VRAM needed', bytesShort(vram)]);
  if (extra !== null && extra > 0) facts.push(['Extra VRAM', bytesShort(extra)]);
  if (hostRam !== null) facts.push(['Host RAM needed', bytesShort(hostRam)]);
  if (disk !== null) facts.push(['Disk needed', bytesShort(disk)]);
  facts.push(['Feasible on this card', feasible === true ? 'yes' : feasible === false ? 'no' : 'not stated']);

  const parts = [
    h('h3', {}, `Requantisation: ${requested} is not in the repository`),
    reason ? richHint(reason) : null,
    kv(facts),
    richHint('This is not free and not instant: it is a separate GPU run that produces a new checkpoint. '
      + 'Manarouter records the recipe and the command; it does not run the quantiser for you.'),
  ];

  const notes = listOfStrings(pick(plan, 'note'));
  if (!notes.length) {
    const single = asString(pick(plan, 'note')).trim();
    if (single) notes.push(single);
  }
  if (notes.length) parts.push(noteList('What the recipe assumes', notes));

  if (paths.length) {
    parts.push(h('h4', {}, 'Paths the recipe offers'));
    for (const path of paths) {
      const steps = listOfStrings(pick(path, 'steps'));
      const pathParts = [
        h('div', { class: 'gpu-inline-label' }, asString(pick(path, 'label', 'path')) || '—',
          asString(pick(path, 'quality')).trim() ? h('span', { class: 'hint' }, ` · ${asString(pick(path, 'quality'))}`) : null),
        h('p', { class: 'hint' }, asString(pick(path, 'produces_note', 'produces')).trim() || '—'),
        kv([
          ['Estimated time', minutesRange(pick(path, 'estimated_minutes_range'), asNumber(pick(path, 'estimated_minutes')))],
          ['VRAM needed', bytesShort(pick(path, 'requires_vram_bytes'))],
          ['Verified for this architecture', pick(path, 'verified_for_this_architecture') === true ? 'yes'
            : pick(path, 'verified_for_this_architecture') === false ? 'no — the recipe is unverified here' : 'not stated'],
        ]),
      ];
      if (steps.length) pathParts.push(h('ol', { class: 'gpu-requant-steps' }, ...steps.map((s) => h('li', {}, ...inlineNodes(s)))));
      parts.push(h('div', { class: 'gpu-requant-path' }, ...pathParts.filter(Boolean)));
    }
  }

  const serve = asString(pick(plan, 'serve_command')).trim();
  if (serve) parts.push(h('p', { class: 'mono gpu-requant-note' }, serve));
  return h('div', { class: 'gpu-requant' }, ...parts.filter(Boolean));
}

function sizingPanel(wizard, ctx) {
  if (!wizard.probe) {
    return h('div', { class: 'gpu-blank' },
      h('h3', {}, 'Sizing needs a probe'),
      hint('The KV pool is computed from the architecture the probe read. Without a probe there is nothing to compute, so this panel does not show an invented pool size.'),
      h('div', { class: 'gpu-actions' }, ghostButton('Go to the model step', () => goToStep(wizard, 'model'))));
  }
  const sizing = objectOf(wizard.sizing);
  if (!sizing) return h('div', { class: 'gpu-blank' }, hint('The sizing model returned no result. Nothing was substituted.'));

  const method = asString(pick(sizing, 'method')).trim();
  const parts = [h('h3', {}, 'Derived sizing')];
  if (method === 'unavailable') {
    parts.push(h('p', { class: 'gpu-unavailable' }, `The KV pool is not estimated: ${asString(pick(sizing, 'reason')).trim() || 'the model gave no reason.'}`));
    const missing = listOfStrings(pick(sizing, 'missing'));
    if (missing.length) {
      parts.push(h('div', { class: 'gpu-unavailable-list' },
        h('h4', {}, 'What is missing'),
        h('ul', {}, ...missing.map((m) => h('li', {}, ...inlineNodes(m))))));
    }
    parts.push(richHint('Unavailable is not zero. The deployment can still be recorded, but the view will not pretend it knows a pool size.'));
    return h('div', { class: 'gpu-sizing' }, ...parts);
  }

  const pool = asNumber(pick(sizing, 'kv_pool_tokens'));
  const poolSource = asString(pick(sizing, 'kv_pool_tokens_source')).trim();
  const perToken = asNumber(pick(sizing, 'kv_bytes_per_token'));
  const perTokenBasis = asString(pick(sizing, 'kv_bytes_per_token_basis')).trim();
  const effective = asNumber(pick(sizing, 'max_num_seqs_effective'));
  const effectiveBasis = asString(pick(sizing, 'max_num_seqs_effective_basis')).trim();
  const maxLen = asNumber(pick(sizing, 'max_model_len'));
  const confidence = asString(pick(sizing, 'confidence')).trim();
  const delta = asNumber(pick(sizing, 'delta_vs_measurement_pct'));
  const measured = objectOf(pick(sizing, 'measured_reference'));

  parts.push(kv([
    ['KV pool', pool === null ? '—' : `${formatCompact(pool)} tokens`],
    ['Pool source', poolSource || 'not reported'],
    ['KV per token', perToken === null ? '—' : `${formatCompact(perToken)} bytes`],
    ['Per-token basis', perTokenBasis || '—'],
    ['Max model length', maxLen === null ? '—' : String(maxLen)],
    ['Effective simultaneous sessions', effective === null ? '—' : String(effective)],
    ['Basis', effectiveBasis || '—'],
    ['Method', `${method || '—'} · confidence ${confidence || '—'}`],
  ]));
  parts.push(richHint('The analytic pool is an estimate. vLLM\'s startup line "GPU KV cache size: N tokens" is authoritative — read it back and override the provider\'s pool with it.'));

  const sessions = listOf(pick(sizing, 'simultaneous_sessions_at')).map(objectOf).filter(Boolean);
  if (sessions.length) {
    parts.push(h('h4', {}, 'Simultaneous sessions by context length'));
    parts.push(dataTable([{ label: 'Context (tokens)', num: true }, { label: 'Sessions', num: true }],
      sessions.map((s) => h('tr', {},
        h('td', { class: 'num' }, asNumber(pick(s, 'context_len', 'context')) === null ? '—' : String(asNumber(pick(s, 'context_len', 'context')))),
        h('td', { class: 'num' }, asNumber(pick(s, 'sessions')) === null ? '—' : String(asNumber(pick(s, 'sessions'))))))));
  }

  if (measured) {
    parts.push(kv([
      ['Measured reference', `${asString(pick(measured, 'model')).trim() || '—'} on ${asString(pick(measured, 'gpu')).trim() || '—'}`],
      ['Measured tokens', asNumber(pick(measured, 'kv_pool_tokens')) === null ? '—' : `${formatCompact(asNumber(pick(measured, 'kv_pool_tokens')))} tokens`],
      ['Delta vs measurement', delta === null ? '—' : `${delta > 0 ? '+' : ''}${delta.toFixed(1)}%`],
      ['Delta at usable VRAM', asNumber(pick(sizing, 'delta_at_usable_vram_pct')) === null ? '—' : `${asNumber(pick(sizing, 'delta_at_usable_vram_pct'))}%`],
    ]));
  } else {
    parts.push(h('p', { class: 'hint' }, 'No measurement of this model on this card is recorded, so there is no measured delta to check the estimate against. The vLLM startup line remains the authority.'));
  }

  const assumptions = listOfStrings(pick(sizing, 'assumptions'));
  if (assumptions.length) parts.push(noteList('What this estimate assumes', assumptions));
  const warnings = listOfStrings(pick(sizing, 'warnings'));
  if (warnings.length) parts.push(noteList('Warnings', warnings, 'gpu-caveat'));
  return h('div', { class: 'gpu-sizing' }, ...parts);
}

function reviewPanel(wizard) {
  const parts = [];
  if (wizard.bundleError) parts.push(failedEndpoint('panel/gpu-setup-model.js deploymentBundle()', wizard.bundleError));
  if (wizard.applyError) parts.push(failedEndpoint('POST /api/gpu/apply (create-deployment)', wizard.applyError));
  const platform = selectedPlatform(wizard);
  parts.push(h('h3', {}, 'The bundle'));
  if (wizard.bundle) {
    parts.push(bundlePanel(wizard.bundle, platform, { honesty: false }));
  } else {
    parts.push(h('div', { class: 'gpu-blank' }, hint('The deployment bundle has not been generated yet. It is generated from the draft; nothing is posted by rendering it.')));
  }

  const payload = objectOf(callModel(wizard, 'applyPayload', null, wizard.draft, wizard.sizing));
  if (payload) {
    const dep = objectOf(pick(payload, 'deployment')) ?? {};
    parts.push(h('h3', {}, 'The request Manarouter will record'));
    parts.push(kv([
      ['Action', asString(pick(payload, 'action'))],
      ['Account', asString(pick(payload, 'account_id')) || '—'],
      ['Repository', asString(pick(dep, 'repo')) || '—'],
      ['Card', `${asString(pick(dep, 'gpu')) || '—'} × ${asNumber(pick(dep, 'gpu_count')) ?? '—'}`],
      ['Weight quant', asString(pick(dep, 'weight_quant')) || '—'],
      ['KV cache dtype', asString(pick(dep, 'kv_cache_dtype')) || '—'],
      ['Mode', asString(pick(dep, 'mode')) || '—'],
      ['Base URL', asString(pick(dep, 'base_url')) || '—'],
    ]));
    parts.push(richHint('The account token is not part of this request: the account was stored separately before any model was chosen.'));
  }
  return h('div', { class: 'gpu-review' }, ...parts.filter(Boolean));
}

// --- interactions ---------------------------------------------------------------------------

function goToStep(wizard, stepId) {
  if (!stepId || wizard.busy || stepId === wizard.current) return;
  const steps = modelSteps(wizard);
  const target = stepIndexOf(steps, stepId);
  const here = stepIndexOf(steps, wizard.current);
  if (target < 0) return;
  if (target > here) {
    if (!validateCurrent(wizard)) return;
    wizard.maxVisited = Math.max(wizard.maxVisited, target);
  }
  setupGo(wizard, setupHref(stepId));
}

function goPrevious(wizard) {
  const previous = stepIdOf(callModel(wizard, 'prevStep', null, wizard.draft, wizard.current));
  if (previous) setupGo(wizard, setupHref(previous));
}

function nextStepId(wizard) {
  return stepIdOf(callModel(wizard, 'nextStep', null, wizard.draft, wizard.current));
}

async function advance(wizard, ctx) {
  if (wizard.busy) return;
  if (!validateCurrent(wizard)) return;
  if (wizard.current === 'account') {
    const stored = await storeAccount(wizard, ctx);
    if (!stored) return;
  }
  if (wizard.current === 'review') {
    await submitDeployment(wizard, ctx, { alreadyValidated: true });
    return;
  }
  const next = nextStepId(wizard);
  if (!next) {
    wizard.callProblems.push({ name: 'nextStep', message: `nextStep() returned no step after ${wizard.current}; the flow cannot advance.` });
    paintSetup(wizard, wizard.mount, ctx, wizard.current);
    return;
  }
  const steps = modelSteps(wizard);
  wizard.maxVisited = Math.max(wizard.maxVisited, stepIndexOf(steps, next));
  setupGo(wizard, setupHref(next));
}

async function storeAccount(wizard, ctx) {
  if (wizard.draft?.reuseAccount === true) {
    const problem = callModel(wizard, 'accountReuseProblem', null, wizard.draft, wizard.accounts);
    if (typeof problem === 'string' && problem) {
      wizard.accountError = new Error(problem);
      paintSetup(wizard, wizard.mount, ctx, wizard.current);
      return false;
    }
    const id = asString(draftGet(wizard.draft, 'accountId')).trim();
    const stored = wizard.accounts.find((a) => sameText(a.id, id)) ?? { id };
    // No request: the credential is already in the vault, and the deployment references the account
    // by id. The signature stops a revisit from taking this path again for a different id.
    wizard.accountSignature = JSON.stringify(['reuse', id, asString(draftGet(wizard.draft, 'platform')).trim()]);
    wizard.accountError = null;
    wizard.accountResult = { account: stored };
    return true;
  }
  const payload = objectOf(callModel(wizard, 'accountApplyPayload', null, wizard.draft));
  if (!payload) {
    wizard.accountError = new Error(
      'panel/gpu-setup-model.js exports no accountApplyPayload(), so the account step cannot be posted. Nothing was sent.');
    paintSetup(wizard, wizard.mount, ctx, wizard.current);
    return false;
  }
  const account = objectOf(pick(payload, 'account')) ?? payload;
  const signature = JSON.stringify([
    asString(pick(account, 'id')).trim(),
    asString(pick(account, 'platform')).trim(),
    asString(pick(account, 'plan')).trim(),
    asString(pick(account, 'token_id')).trim(),
    asString(pick(account, 'token_secret')),
    asString(pick(account, 'hf_token')),
  ]);
  if (wizard.accountSignature === signature) return true;

  wizard.busy = true;
  wizard.accountError = null;
  paintSetup(wizard, wizard.mount, ctx, wizard.current);
  try {
    const result = await api.post('/api/gpu/apply', payload);
    wizard.accountSignature = signature;
    wizard.accountResult = objectOf(result) ?? { account };
    wizard.dirty = true;
    if (ctx && typeof ctx.toast?.success === 'function') ctx.toast.success('Account stored in the vault.');
    return true;
  } catch (err) {
    wizard.accountError = err;
    if (ctx && typeof ctx.toast?.error === 'function') ctx.toast.error('The account was not stored', err);
    return false;
  } finally {
    wizard.busy = false;
    if (wizard.mount) paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
  }
}

async function runProbe(wizard, ctx) {
  if (wizard.busy) return;
  const repo = asString(draftGet(wizard.draft, 'repo')).trim();
  if (!repo) {
    if (ctx && typeof ctx.toast?.warning === 'function') ctx.toast.warning('Paste a model repository first.');
    return;
  }
  const useHfToken = Boolean(asString(draftGet(wizard.draft, 'hfToken')).trim())
    || boolOf(callModel(wizard, 'requiresHfToken', false, wizard.probe));
  const body = {
    repo,
    revision: asString(draftGet(wizard.draft, 'revision')).trim(),
    account_id: asString(draftGet(wizard.draft, 'accountId')).trim(),
    use_hf_token: useHfToken,
  };
  wizard.busy = true;
  wizard.probeError = null;
  paintSetup(wizard, wizard.mount, ctx, wizard.current);
  try {
    const result = await api.post('/api/gpu/probe', body);
    wizard.probe = objectOf(result) ?? {};
    const merged = callModel(wizard, 'applyProbe', wizard.draft, wizard.draft, wizard.probe, wizard.catalog);
    if (merged && typeof merged === 'object') wizard.draft = merged;
    draftSet(wizard.draft, 'probe', wizard.probe);
    wizard.sizing = null;
    wizard.bundle = null;
    wizard.validation = null;
    if (ctx && typeof ctx.toast?.success === 'function') ctx.toast.success('Repository probed.');
  } catch (err) {
    wizard.probeError = err;
    if (ctx && typeof ctx.toast?.error === 'function') ctx.toast.error('The repository could not be probed', err);
  } finally {
    wizard.busy = false;
    if (wizard.mount) paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
  }
}

async function submitDeployment(wizard, ctx, { alreadyValidated = false } = {}) {
  if (wizard.busy) return;
  if (!alreadyValidated && !validateCurrent(wizard)) return;
  wizard.busy = true;
  wizard.applyError = null;
  paintSetup(wizard, wizard.mount, ctx, wizard.current);
  try {
    refreshSizing(wizard);
    refreshBundle(wizard);
    const payload = objectOf(callModel(wizard, 'applyPayload', null, wizard.draft, wizard.sizing));
    if (!payload) {
      throw new Error('panel/gpu-setup-model.js exports no applyPayload(), so no deployment request could be built. Nothing was sent.');
    }
    const result = await api.post('/api/gpu/apply', payload);
    wizard.applied = objectOf(result) ?? { ok: true };
    wizard.dirty = false;
    draftSet(wizard.draft, 'tokenSecret', '');
    draftSet(wizard.draft, 'hfToken', '');
    if (ctx && typeof ctx.toast?.success === 'function') {
      ctx.toast.success('Deployment recorded. Run the command below to bring it up.');
    }
  } catch (err) {
    wizard.applyError = err;
    if (ctx && typeof ctx.toast?.error === 'function') ctx.toast.error('The deployment was not recorded', err);
  } finally {
    wizard.busy = false;
    if (wizard.mount) paintSetup(wizard, wizard.mount, wizard.ctx, wizard.current);
  }
}

function renderCallProblems(wizard) {
  return h('div', { class: 'gpu-failed', role: 'status' },
    h('h3', {}, 'The setup model did not answer'),
    h('p', { class: 'hint' }, 'These are gaps in the view-model interface, not operator mistakes. Nothing was substituted.'),
    h('ul', { class: 'gpu-notes' }, ...wizard.callProblems.map((p) => h('li', { class: 'mono' }, p.message))));
}

function appliedBlock(wizard, ctx) {
  const result = objectOf(wizard.applied) ?? {};
  const dep = objectOf(pick(result, 'deployment')) ?? result;
  const providerId = asString(pick(result, 'provider_id', 'providerId', 'id')).trim()
    || asString(pick(dep, 'provider_id', 'providerId', 'id')).trim();
  const parts = [
    h('h3', {}, 'Recorded in Manarouter'),
    kv([
      ['Provider id', providerId || '—'],
      ['Repository', asString(pick(dep, 'repo')).trim() || asString(draftGet(wizard.draft, 'repo'))],
      ['Card', asString(pick(dep, 'gpu')).trim() || asString(draftGet(wizard.draft, 'gpu'))],
    ]),
    richHint('This is a record, not a running container. Run the exact command in the bundle above. Modal publishes no deployment REST API, so that command is the deployment.'),
    h('div', { class: 'gpu-actions' }, button('Back to GPU rentals', () => {
      wizard.dirty = false;
      ctxNavigate('#/gpu-rentals');
    }, { className: 'btn btn-primary' })),
  ];
  if (wizard.applyError) parts.push(failedEndpoint('POST /api/gpu/apply', wizard.applyError));
  return h('div', { class: 'gpu-applied' }, ...parts.filter(Boolean));
}

function setupDestructiveConfirm(ctx, opts) {
  if (typeof ctx?.confirm !== 'function') return Promise.resolve(false);
  return Promise.resolve(ctx.confirm(opts)).then(Boolean, () => false);
}

async function discardSetup(wizard, ctx) {
  const leave = await setupDestructiveConfirm(ctx, {
    title: 'Discard this deployment setup?',
    message: 'The draft is discarded. An account that was already stored stays in the vault; this does not delete it.',
    confirmLabel: 'Discard setup',
    cancelLabel: 'Keep editing',
    danger: true,
  });
  if (!leave) return;
  retractSetupGuard(wizard, ctx);
  setupSession = null;
  if (ctx && typeof ctx.toast?.info === 'function') ctx.toast.info('Setup discarded.');
  ctxNavigate('#/gpu-rentals');
}

async function leaveSetup(wizard, ctx) {
  if (!wizard.dirty) {
    retractSetupGuard(wizard, ctx);
    ctxNavigate('#/gpu-rentals');
    return;
  }
  const leave = await setupDestructiveConfirm(ctx, LEAVE_CONFIRM);
  if (!leave) return;
  retractSetupGuard(wizard, ctx);
  setupSession = null;
  ctxNavigate('#/gpu-rentals');
}

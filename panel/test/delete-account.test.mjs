// Pins what the operator is told when the last account of a provider is deleted.
//
// The conductor's DELETE /api/providers/{id}/accounts/{account_id} now answers 200 for that case
// instead of 400: the resulting configuration keeps the provider with no accounts, which can never
// serve, so the validator reports it as an *advisory* and the response carries it back:
//
//   200 {"deleted":"…","warnings":["provider acme: provider has no accounts, so it can never
//        serve a request"]}
//
// A success body can therefore carry warnings, and a success message must never hide one. What is
// pinned here:
//
//   * warnings present  → the delete is acknowledged AND every warning reaches the operator;
//   * warnings absent or empty → a plain success and no warning (nothing invented);
//   * a 400 → the server's error message still reaches the error path, unchanged;
//   * the repaint that follows the delete does not take the warning off the screen.
//
// No jsdom (the panel has no dependencies): the same element stub as providers-refresh.test.mjs,
// extended with firstElementChild (the toast container caps its stack) and qualified selectors used
// by placeDetailRow.
//
// Run: node --test panel/test/delete-account.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

// ------------------------------------------------------------------ DOM stubs

function makeEl(tag) {
  const el = {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    dataset: {},
    listeners: {},
    className: '',
    id: '',
    textContent: '',
    value: undefined,
    style: {},
    checked: false,
    disabled: false,
    hidden: false,
    isConnected: true,
    classList: {
      add() {},
      remove() {},
      toggle() {},
      contains(c) { return String(el.className).split(/\s+/).includes(c); },
    },
    get firstElementChild() { return el.children[0] ?? null; },
    setAttribute(k, v) {
      el.attrs[k] = String(v);
      if (k === 'class') el.className = String(v);
      if (k === 'id') el.id = String(v);
      if (k === 'value') el.value = String(v);
    },
    getAttribute(k) { return Object.hasOwn(el.attrs, k) ? el.attrs[k] : null; },
    removeAttribute(k) { delete el.attrs[k]; },
    addEventListener(type, fn) { (el.listeners[type] ||= []).push(fn); },
    removeEventListener() {},
    append(...kids) { for (const kid of kids) { if (kid) kid.parent = el; el.children.push(kid); } },
    appendChild(kid) { kid.parent = el; el.children.push(kid); return kid; },
    prepend(...kids) { for (const kid of kids) { if (kid) kid.parent = el; el.children.unshift(kid); } },
    replaceChildren(...kids) {
      el.children = kids.filter((kid) => kid != null);
      for (const kid of el.children) if (kid && typeof kid === 'object') kid.parent = el;
    },
    after(node) {
      const parent = el.parent;
      if (!parent) return;
      const at = parent.children.indexOf(el);
      parent.children.splice(at + 1, 0, node);
      if (node && typeof node === 'object') node.parent = parent;
    },
    remove() {
      const parent = el.parent;
      if (!parent) return;
      const at = parent.children.indexOf(el);
      if (at !== -1) parent.children.splice(at, 1);
      el.parent = null;
    },
    focus() {},
    scrollIntoView() {},
    contains(node) {
      if (node === el) return true;
      return el.children.some((kid) => kid && kid.nodeType === 1 && typeof kid.contains === 'function' && kid.contains(node));
    },
    querySelector(sel) { return queryAll(el, sel)[0] ?? null; },
    querySelectorAll(sel) { return queryAll(el, sel); },
  };
  return el;
}

function selectorMatches(node, selector) {
  if (!node || node.nodeType !== 1) return false;
  const sel = selector.trim();
  if (sel.includes(' > ') || sel.includes(',')) return false; // handled by queryAll / not used
  // Attribute selector, with or without a value: `tr[data-provider-id="acme"]`.
  const attrMatch = /^([\w-]+)?\[([\w-]+)(?:="([^"]*)")?\]$/.exec(sel);
  if (attrMatch) {
    const tag = attrMatch[1] ?? '';
    const attrKey = attrMatch[2];
    if (tag && node.tagName !== tag.toUpperCase()) return false;
    if (attrMatch[3] === undefined) return node.getAttribute?.(attrKey) != null;
    const actual = attrKey === 'class' ? node.className : node.getAttribute?.(attrKey);
    return String(actual ?? '') === attrMatch[3];
  }
  if (sel.startsWith('.')) return String(node.className ?? '').split(/\s+/).includes(sel.slice(1));
  if (/^[\w-]+$/.test(sel)) return node.tagName === sel.toUpperCase();
  return false;
}

/** Descendants only (never the node itself), like Element.querySelectorAll. */
function queryAll(node, selector) {
  const out = [];
  const gtIndex = selector.indexOf(' > ');
  if (gtIndex !== -1) {
    const left = selector.slice(0, gtIndex);
    const right = selector.slice(gtIndex + 3);
    const leftMatches = [];
    const find = (n) => {
      if (!n || n.nodeType !== 1) return;
      if (selectorMatches(n, left)) leftMatches.push(n);
      for (const kid of n.children) find(kid);
    };
    for (const kid of node.children) find(kid);
    for (const lm of leftMatches) {
      for (const kid of lm.children ?? []) if (kid && selectorMatches(kid, right)) out.push(kid);
    }
    return out;
  }
  const walk = (n) => {
    for (const kid of n.children) {
      if (kid && selectorMatches(kid, selector)) out.push(kid);
      if (kid && kid.nodeType === 1) walk(kid);
    }
  };
  walk(node);
  return out;
}

// ------------------------------------------------------------------ globals

globalThis.document = {
  createElement: makeEl,
  createElementNS: (_ns, tag) => makeEl(tag),
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
  querySelector(sel) { return queryAll(this.body, sel)[0] ?? null; },
  querySelectorAll(sel) { return queryAll(this.body, sel); },
};
globalThis.CSS = { escape: (value) => String(value).replace(/["\\]/g, '\\$&') };
globalThis.window = { addEventListener() {}, open() {} };
globalThis.requestAnimationFrame = (cb) => { cb(0); return 0; };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
const flush = async (times = 12) => { for (let i = 0; i < times; i += 1) await settle(); };

// ------------------------------------------------------------------ helpers

/** All text under a node, since the stub does not aggregate textContent. */
function textOf(node) {
  if (!node || typeof node !== 'object') return String(node ?? '');
  if (node.nodeType === 3) return String(node.textContent ?? '');
  let out = typeof node.textContent === 'string' ? node.textContent : '';
  for (const kid of node.children ?? []) out += textOf(kid);
  return out;
}

function findOne(node, tag, out = null) {
  if (!node || node.nodeType !== 1) return out;
  if (node.tagName === tag) return node;
  for (const kid of node.children) {
    out = findOne(kid, tag, out);
    if (out) return out;
  }
  return out;
}

/** The first BUTTON whose aria-label starts with `label`. */
function findByAriaLabel(node, label) {
  if (!node || node.nodeType !== 1) return null;
  if (node.tagName === 'BUTTON' && String(node.getAttribute('aria-label') ?? '').startsWith(label)) return node;
  for (const kid of node.children) {
    const hit = findByAriaLabel(kid, label);
    if (hit) return hit;
  }
  return null;
}

function provider(id) {
  return {
    id,
    label: `${id} label`,
    dialect: 'open-ai',
    enabled: true,
    accounts: [{ id: `${id}-main`, label: 'main', enabled: true }],
    models: [{ id: 'model-a' }],
  };
}

/** A recording ctx.toast: every call is kept with its kind, message and error. */
function recordingToasts() {
  const calls = [];
  const record = (kind) => (message, err) => { calls.push({ kind, message, err }); };
  return {
    calls,
    api: {
      success: record('success'),
      error: record('error'),
      warning: record('warning'),
      info: record('info'),
    },
    of(kind) { return calls.filter((call) => call.kind === kind); },
  };
}

/**
 * Mount the view, open the provider's inline detail panel, and click the account's Delete button.
 *
 * `deleteResponse` is what the DELETE request answers with — a fetch Response shape. Every other
 * request (the quota read during renderDetail) answers with an empty success so it cannot be
 * mistaken for the delete.
 */
async function mountAndDelete({ deleteResponse, toast = recordingToasts(), onLoad = null }) {
  globalThis.fetch = async (_url, init) => {
    if ((init?.method ?? 'GET') === 'DELETE') return deleteResponse();
    return { ok: true, status: 200, json: async () => ({}) };
  };

  const { renderProviders } = await import('../view-providers.js');

  const mount = makeEl('div');
  const data = { current: [provider('acme')] };
  let loads = 0;
  const ctx = {
    loadProviders: async () => {
      loads += 1;
      if (onLoad) onLoad(loads);
      return data.current;
    },
    onHealth: (cb) => { cb({ snapshot: { digest: '1111111111111111' } }); return () => {}; },
    toast: toast.api,
    confirm: async () => true,
  };

  await renderProviders(mount, ctx);
  await settle();
  // The view lives under document.body so document.querySelector sees it, as in the real page.
  document.body.appendChild(mount);

  const row = findOne(mount, 'TBODY')?.children
    ?.find((tr) => tr?.getAttribute?.('data-provider-id') === 'acme');
  assert.ok(row, 'row for acme should exist');

  const openBtn = row.querySelector('.provider-open');
  assert.ok(openBtn, 'the row should carry its Open button');
  openBtn.listeners.click[0]({ stopPropagation() {} });
  await flush();

  const body = queryAll(mount, '.drawer-body')[0];
  assert.ok(body, 'the provider detail drawer should be open');

  const delBtn = findByAriaLabel(body, 'Delete account');
  assert.ok(delBtn, 'the account row should carry its Delete account button');

  const tbodyBefore = findOne(mount, 'TBODY');
  delBtn.listeners.click[0]({ stopPropagation() {} });
  await flush();

  return { toasts: toast, mount, tbodyBefore, loads: () => loads, body };
}

// ------------------------------------------------------------------ tests

test('a body carrying warnings acknowledges the delete and surfaces the advisory', async () => {
  const warnings = ['provider acme: provider has no accounts, so it can never serve a request'];
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({ ok: true, status: 200, json: async () => ({ deleted: 'acme-main', warnings }) }),
  });

  const success = toasts.of('success');
  assert.equal(success.length, 1, 'the delete itself must still be acknowledged');
  assert.equal(success[0].message, 'Account deleted.');

  const warned = toasts.of('warning');
  assert.equal(warned.length, 1, 'the advisory must be surfaced, not buried under the success');
  assert.equal(warned[0].message, warnings[0], 'the warning text is the server’s, unedited');
  assert.equal(toasts.of('error').length, 0, 'a 200 is not an error');
});

test('every warning is surfaced, not just the first', async () => {
  const warnings = [
    'provider acme: provider has no accounts, so it can never serve a request',
    'provider acme: no models are enabled for this provider',
  ];
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({ ok: true, status: 200, json: async () => ({ deleted: 'acme-main', warnings }) }),
  });

  assert.deepEqual(toasts.of('warning').map((call) => call.message), warnings);
  assert.equal(toasts.of('success').length, 1);
});

test('an empty warnings array leaves a plain success', async () => {
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({ ok: true, status: 200, json: async () => ({ deleted: 'acme-main', warnings: [] }) }),
  });

  assert.equal(toasts.of('success').length, 1);
  assert.equal(toasts.of('success')[0].message, 'Account deleted.');
  assert.equal(toasts.of('warning').length, 0, 'an empty advisory list invents nothing');
});

test('a body with no warnings key leaves a plain success', async () => {
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({ ok: true, status: 200, json: async () => ({ deleted: 'acme-main' }) }),
  });

  assert.equal(toasts.of('success').length, 1);
  assert.equal(toasts.of('success')[0].message, 'Account deleted.');
  assert.equal(toasts.of('warning').length, 0);
});

test('a body with no JSON at all still acknowledges the delete', async () => {
  // api.del() yields undefined when the response body is not JSON; that must not throw.
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({ ok: true, status: 200, json: async () => { throw new SyntaxError('no body'); } }),
  });

  assert.equal(toasts.of('success').length, 1);
  assert.equal(toasts.of('warning').length, 0);
  assert.equal(toasts.of('error').length, 0);
});

test('a 400 still surfaces the server’s error message on the error path', async () => {
  const details = ['provider acme: the configuration is not valid'];
  const { toasts } = await mountAndDelete({
    deleteResponse: () => ({
      ok: false,
      status: 400,
      json: async () => ({
        error: { code: 'validation_failed', message: 'the configuration is not valid', details },
      }),
    }),
  });

  const errors = toasts.of('error');
  assert.equal(errors.length, 1, 'a failed delete must reach the error path');
  assert.equal(errors[0].message, 'Delete failed', 'the message the panel has always used');
  assert.equal(errors[0].err?.status, 400);
  assert.equal(errors[0].err?.message, 'the configuration is not valid', 'the server’s own words');
  assert.deepEqual(errors[0].err?.details, details, 'validation details still travel in full');
  assert.equal(toasts.of('success').length, 0, 'a 400 must not be reported as a success');
  assert.equal(toasts.of('warning').length, 0);
});

test('the repaint after the delete does not take the warning off the screen', async () => {
  // The real toast API, in a container that stands in for #toasts — which index.html keeps outside
  // the view's mount — so this proves the node the operator can see survives reload(true).
  const realSetTimeout = globalThis.setTimeout;
  // The toast TTLs are 6–9 s and would hold the test process open; they are not what is under test.
  globalThis.setTimeout = (fn, ms, ...rest) => (Number(ms) >= 1000 ? { unref() {} } : realSetTimeout(fn, ms, ...rest));
  try {
    const toastsEl = makeEl('div');
    toastsEl.id = 'toasts';
    document.body.appendChild(toastsEl);
    const { createToasts } = await import('../toast.js');
    const toastApi = createToasts(toastsEl);

    const warnings = ['provider acme: provider has no accounts, so it can never serve a request'];
    let toastNodesDuringReload = null;
    const { mount, tbodyBefore } = await mountAndDelete({
      toast: { api: toastApi },
      // reload(true) is the repaint: snapshot what is on screen at the moment it starts.
      onLoad: (loads) => {
        if (loads >= 2) toastNodesDuringReload = toastsEl.children.slice();
      },
      deleteResponse: () => ({ ok: true, status: 200, json: async () => ({ deleted: 'acme-main', warnings }) }),
    });

    const warningNode = toastsEl.children
      .find((node) => String(node.className ?? '').includes('toast-warning'));
    assert.ok(warningNode, 'the advisory should be on screen as a warning toast');
    assert.match(textOf(warningNode), /provider has no accounts, so it can never serve a request/);

    const successNode = toastsEl.children
      .find((node) => String(node.className ?? '').includes('toast-success'));
    assert.ok(successNode, 'the delete is still acknowledged');
    assert.match(textOf(successNode), /Account deleted\./);

    assert.ok(toastNodesDuringReload, 'the delete should have triggered a repaint');
    assert.ok(toastNodesDuringReload.includes(warningNode),
      'the warning must already be on screen when the repaint starts');

    // The repaint really happened: a fresh table body replaced the one the delete was clicked in.
    const tbodyAfter = findOne(mount, 'TBODY');
    assert.ok(tbodyAfter, 'the repainted table should have a body');
    assert.notEqual(tbodyAfter, tbodyBefore, 'reload(true) must rebuild the table');

    // …and the warning node is still the same element, still in the toast container.
    assert.ok(toastsEl.children.includes(warningNode),
      'the repaint must not remove the warning it just reported');
    assert.equal(warningNode.parent, toastsEl, 'the warning still belongs to the toast area');
    assert.match(textOf(warningNode), /can never serve a request/);
  } finally {
    globalThis.setTimeout = realSetTimeout;
  }
});
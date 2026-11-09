// Pins the two ways the Providers tab could show a list that did not contain a provider the API
// was serving:
//
//   * it read the list once and reused that cached array for the life of the page, so anything
//     written afterwards — `mr provider add`, an account added from another tab, an
//     `mr migrate` — stayed invisible until the operator reloaded the page;
//   * nothing refreshed it while the tab was open, so a provider added behind the panel's back
//     needed a manual Refresh.
//
// The view now asks for a fresh read on mount and re-reads when the 5 s health poll reports a new
// routing-snapshot digest, which the control plane rebuilds on every configuration write.
//
// No jsdom here (the panel has no dependencies): the same ~60-line element stub as dom.test.mjs,
// extended with the three things this view reads — childElementCount-free traversal, contains(),
// and document.activeElement.
//
// Run: node --test panel/test/providers-refresh.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

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
    classList: {
      add() {},
      remove() {},
      toggle() {},
      contains(c) { return String(el.className).split(/s+/).includes(c); },
    },
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
    append(...kids) { for (const kid of kids) el.children.push(kid); },
    appendChild(kid) { el.children.push(kid); return kid; },
    prepend(...kids) { el.children.unshift(...kids); },
    replaceChildren(...kids) { el.children = kids.filter((kid) => kid != null); },
    remove() {},
    focus() {},
    contains(node) {
      if (node === el) return true;
      return el.children.some((kid) => kid && kid.nodeType === 1 && typeof kid.contains === 'function' && kid.contains(node));
    },
    querySelector() { return null; },
    querySelectorAll() { return []; },
  };
  return el;
}

const initialHealth = { snapshot: { digest: '1111111111111111' } };

globalThis.document = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
globalThis.window = { addEventListener() {} };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

const { renderProviders } = await import('../view-providers.js');

function provider(id, extra = {}) {
  return {
    id,
    label: `${id} label`,
    dialect: 'open-ai',
    enabled: true,
    accounts: [{ id: `${id}-main`, label: 'main', enabled: true }],
    models: [{ id: 'model-a' }],
    ...extra,
  };
}

const allIds = [
  'antigravity', 'cheaperinference', 'commandcode', 'commandcodego', 'freetheai', 'infron',
  'local8100', 'moonmath', 'nvidia', 'onerouter', 'openai-codex', 'opencode-go', 'ovh',
  'tokenharbor', 'uncloseai', 'vllm',
];

/** Mount the real view against a controllable API and health stream. */
async function mountView(payload) {
  const data = { current: payload };
  const calls = [];
  const mount = makeEl('div');
  let healthCb = null;
  const ctx = {
    loadProviders: async (force) => { calls.push(force === true); return data.current; },
    // app.js replays the latest health reading to a new subscriber; the view's baseline digest
    // comes from that replay, so the stub has to do the same.
    onHealth(cb) { healthCb = cb; cb(initialHealth); return () => {}; },
    toast: { success() {}, error() {}, warning() {}, info() {} },
  };
  await renderProviders(mount, ctx);
  return {
    mount,
    data,
    calls,
    tick(health) { healthCb(health); },
    rows() { return renderedIds(mount); },
  };
}

/** Depth-first search for the first element with this tag name. */
function findOne(node, tag) {
  if (!node || node.nodeType !== 1) return null;
  if (node.tagName === tag) return node;
  for (const kid of node.children) {
    const hit = findOne(kid, tag);
    if (hit) return hit;
  }
  return null;
}

/** The provider ids the table is currently showing, in row order. */
function renderedIds(mount) {
  const table = findOne(mount, 'TABLE');
  const tbody = table && findOne(table, 'TBODY');
  if (!tbody) return [];
  return tbody.children.map((tr) => {
    // Rows carry their provider id because the first cell is now the pin control, not the name.
    const id = tr.getAttribute?.('data-provider-id');
    if (id) return id;
    const firstCell = tr.children[0];
    const idText = firstCell?.children?.[0];
    return idText ? String(idText.textContent) : '';
  });
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test('every provider the API returns gets a row, including the last alphabetically', async () => {
  const view = await mountView(allIds.map((id) => provider(id)));
  assert.deepEqual(view.rows(), [...allIds].sort((a, b) => a.localeCompare(b)));
  // The provider that was reported missing from the tab: it was in the payload and in the table,
  // it was just twelve rows down a list the panel never re-read.
  assert.ok(view.rows().includes('opencode-go'));
});

test('the tab asks for a fresh read when it mounts', async () => {
  const view = await mountView([provider('alpha')]);
  assert.deepEqual(view.calls, [true], 'a cached read would keep a provider added since page load invisible');
});

test('an unchanged digest costs no request', async () => {
  const view = await mountView([provider('alpha')]);
  view.tick({ snapshot: { digest: '1111111111111111' } });
  await settle();
  assert.deepEqual(view.calls, [true]);
});

test('a new digest pulls the new provider in without a manual refresh', async () => {
  const view = await mountView([provider('alpha')]);
  view.data.current = [provider('alpha'), provider('zzz-written-by-the-cli')];

  view.tick({ snapshot: { digest: '2222222222222222' } });
  await settle();

  assert.deepEqual(view.calls, [true, true], 'one extra forced read');
  assert.deepEqual(view.rows(), ['alpha', 'zzz-written-by-the-cli']);
});

test('a digest change waits while the operator is typing in the table', async () => {
  const view = await mountView([provider('alpha')]);
  const input = makeEl('input');
  view.mount.append(input);
  document.activeElement = input;
  try {
    view.tick({ snapshot: { digest: '3333333333333333' } });
    await settle();
    assert.deepEqual(view.calls, [true], 'a redraw here would discard the half-typed value');
  } finally {
    document.activeElement = null;
  }

  // The next write is picked up as usual.
  view.tick({ snapshot: { digest: '4444444444444444' } });
  await settle();
  assert.deepEqual(view.calls, [true, true]);
});

test('a change held back for an open form lands once the form is gone', async () => {
  const view = await mountView([provider('alpha')]);
  const input = makeEl('input');
  view.mount.append(input);
  document.activeElement = input;
  try {
    view.tick({ snapshot: { digest: '5555555555555555' } });
    await settle();
    assert.deepEqual(view.calls, [true], 'deferred, not dropped');
  } finally {
    document.activeElement = null;
  }

  // Same digest as the deferred tick: the retry has to remember the change, because a quiet health
  // poll looks exactly like this one.
  view.tick({ snapshot: { digest: '5555555555555555' } });
  await settle();
  assert.deepEqual(view.calls, [true, true], 'the deferred change is applied on a later tick');
});

test('a health reading without a digest (control plane down) leaves the table alone', async () => {
  const view = await mountView([provider('alpha')]);
  view.tick({});
  view.tick({ snapshot: {} });
  await settle();
  assert.deepEqual(view.calls, [true]);
});

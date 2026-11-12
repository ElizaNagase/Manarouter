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
    remove() {},
    focus() {},
    scrollIntoView() {},
    isConnected: true,
    contains(node) {
      if (node === el) return true;
      return el.children.some((kid) => kid && kid.nodeType === 1 && typeof kid.contains === 'function' && kid.contains(node));
    },
    querySelector(sel) { return queryAll(el, sel)[0] ?? null; },
    querySelectorAll(sel) { return queryAll(el, sel); },
  };
  return el;
}

/**
 * The handful of selectors this view actually asks for: `tbody`, `.drawer-body`, `tag[attr="v"]`.
 * The provider drawer is inserted under its own row, so a stub that answered every selector with
 * null would leave the detail detached and any assertion about it would pass vacuously.
 */
function selectorMatches(node, selector) {
  if (!node || node.nodeType !== 1) return false;
  const sel = selector.trim();
  const attr = /\[([\w-]+)(?:="([^"]*)")?\]/.exec(sel);
  let rest = sel;
  const wanted = {};
  if (attr) {
    wanted[attr[1]] = attr[2] ?? '';
    rest = sel.replace(attr[0], '');
  }
  const classes = [...rest.matchAll(/\.([\w-]+)/g)].map((m) => m[1]);
  const tag = rest.replace(/\.[\w-]+/g, '').replace(/[#\[\]].*$/, '');
  if (tag && node.tagName !== tag.toUpperCase()) return false;
  for (const cls of classes) {
    if (!String(node.className ?? '').split(/\s+/).includes(cls)) return false;
  }
  for (const [key, value] of Object.entries(wanted)) {
    const actual = key === 'class' ? node.className : node.getAttribute?.(key);
    if (String(actual ?? '') !== value) return false;
  }
  return true;
}

function queryAll(node, selector, out = []) {
  if (!node || node.nodeType !== 1) return out;
  for (const kid of node.children) {
    if (selectorMatches(kid, selector)) out.push(kid);
    queryAll(kid, selector, out);
  }
  return out;
}

const initialHealth = { snapshot: { digest: '1111111111111111' } };

globalThis.document = {
  createElement: makeEl,
  // The provider table's pin and move controls are inline SVG, as in the real panel.
  createElementNS: (_ns, tag) => makeEl(tag),
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
// The drawer addresses its row by a CSS selector built from the provider id.
globalThis.CSS = { escape: (value) => String(value).replace(/["\\]/g, '\\$&') };
globalThis.window = { addEventListener() {} };
// Opening a provider detail focuses the panel and scrolls it into view on the next frame.
globalThis.requestAnimationFrame = (cb) => { cb(0); return 0; };
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

/** Every element in the tree whose aria-label matches, depth first. */
function findAllLabelled(node, pattern, out = []) {
  if (!node || node.nodeType !== 1) return out;
  const label = node.getAttribute?.('aria-label');
  if (label && pattern.test(label)) out.push(node);
  for (const kid of node.children) findAllLabelled(kid, pattern, out);
  return out;
}

/** The first element carrying this class. `h()` sets `className`, not an attribute. */
function findByClass(node, cls, out = null) {
  if (!node || node.nodeType !== 1) return out;
  if (String(node.className ?? '').split(' ').includes(cls) && node.tagName === 'BUTTON') return node;
  for (const kid of node.children) {
    out = findByClass(kid, cls, out);
    if (out) return out;
  }
  return out;
}

/** Click the "Open" action of the row for `id`, which is how the detail becomes visible. */
async function openDetail(view, id) {
  const table = findOne(view.mount, 'TABLE');
  const row = (findOne(table, 'TBODY')?.children ?? []).find(
    (tr) => tr.getAttribute?.('data-provider-id') === id,
  );
  assert.ok(row, `no row for ${id}`);
  const button = findByClass(row, 'provider-open');
  assert.ok(button, `no Open button in the ${id} row`);
  button.listeners.click[0]({ stopPropagation() {} });
  await settle();
}

test('the placeholder editor appears exactly where a base URL names one', async () => {
  // Cloudflare addresses its account inside the path and Snowflake inside the hostname; an account
  // whose provider spells its endpoint without a placeholder has nothing to fill in, so offering the
  // editor there would be a button that does nothing. Before the editor existed the only way to set
  // these values was to hand-edit config.json.
  const templated = provider('cloudflare-ai', {
    base_url: 'https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1',
    accounts: [
      { id: 'cf-main', label: 'main', enabled: true },
      { id: 'cf-second', label: 'second', enabled: true },
    ],
  });
  const plain = provider('plain', { base_url: 'https://api.openai.com/v1' });
  const view = await mountView([templated, plain]);

  // Nothing is open yet: the editor lives in the account table, which only exists for the selected
  // provider.
  assert.equal(findAllLabelled(view.mount, /URL placeholders/).length, 0);

  await openDetail(view, 'cloudflare-ai');
  const templatedButtons = findAllLabelled(view.mount, /^Edit URL placeholders for account /);
  assert.deepEqual(
    templatedButtons.map((b) => b.getAttribute('aria-label')).sort(),
    ['Edit URL placeholders for account cf-main', 'Edit URL placeholders for account cf-second'],
    'one editor per account, named by account so a screen reader can tell them apart',
  );
  // The title says which names the endpoint needs, which is the whole reason the button exists.
  assert.match(templatedButtons[0].getAttribute('title'), /\{account_id\}/);

  // The drawer is replaced when another provider is opened, so this also shows the control is
  // derived from the provider in front of the operator and not left over from the last one.
  await openDetail(view, 'plain');
  assert.equal(
    findAllLabelled(view.mount, /URL placeholders/).length,
    0,
    'a provider that spells its endpoint without a placeholder gets no editor',
  );
});

test('an account with placeholders keeps its editor even when the URL is no longer templated', async () => {
  // A base URL can be edited after the values are set (or the provider replaced), and silently
  // hiding the editor then would leave the operator no way to see or clear what is stored.
  const retired = provider('retired', {
    base_url: 'https://api.example.test/v1',
    accounts: [{ id: 'old', label: 'old', enabled: true, placeholders: { account_id: 'abc123' } }],
  });
  const view = await mountView([retired]);
  await openDetail(view, 'retired');
  assert.equal(findAllLabelled(view.mount, /^Edit URL placeholders for account old$/).length, 1);
});

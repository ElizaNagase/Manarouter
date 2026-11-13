// Contract checks for the provider-page ("screen") sign-in in panel/view-providers.js.
//
// Cursor's login does not come back to this router and has no code to retype: the operator opens
// the provider's page, and completion is discovered by polling. That makes the flow a wire
// contract — start, poll, done — with three things worth pinning:
//
//   * Connect POSTs /api/oauth/{platform}/screen with the provider id and opens the returned page
//     in a new tab, offering a link to reopen it if the tab is lost,
//   * completion is learned from /poll and never from /complete, because nothing arrives here,
//   * a "failed" poll surfaces the server's message instead of polling on, and a single wire error
//     does not end the flow while the session is still alive.
//
// There is no jsdom in this panel, so this file installs the same ~40-line element stub as
// dom.test.mjs and drives the exported form directly.
//
// Run: node --test panel/test/oauth-screen.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

function makeEl(tag) {
  return {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    listeners: {},
    className: '',
    id: '',
    hidden: false,
    disabled: false,
    value: '',
    textContent: '',
    setAttribute(k, v) {
      this.attrs[k] = String(v);
      if (k === 'class') this.className = String(v);
      if (k === 'id') this.id = String(v);
      if (k === 'hidden') this.hidden = true;
    },
    getAttribute(k) {
      return Object.hasOwn(this.attrs, k) ? this.attrs[k] : null;
    },
    addEventListener(type, fn) {
      (this.listeners[type] ||= []).push(fn);
    },
    append(...kids) {
      for (const kid of kids) this.children.push(kid);
    },
    replaceChildren(...kids) {
      this.children = [];
      this.append(...kids);
    },
    focus() {
      document.activeElement = this;
    },
  };
}

const opened = [];
globalThis.document = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
globalThis.window = {
  addEventListener() {},
  open(url, target) {
    opened.push({ url, target });
    return { closed: false };
  },
};

// The flow waits a second between polls. This test is about the sequence, not the cadence (which
// the server owns and which mr-core's session tests pin), so timers fire as microtasks.
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn) => {
  queueMicrotask(fn);
  return 1;
};

const { buildOAuthAccountForm } = await import('../view-providers.js');
const { api } = await import('../api.js');

function walk(node, out = []) {
  out.push(node);
  for (const kid of node.children ?? []) if (kid.nodeType === 1) walk(kid, out);
  return out;
}

const textOf = (node) =>
  node.nodeType === 3 ? node.textContent : (node.children ?? []).map(textOf).join('');

function makeCtx() {
  const successes = [];
  const errors = [];
  return {
    successes,
    errors,
    toast: {
      success: (message) => successes.push(message),
      warning() {},
      error: (message, err) => errors.push({ message, err }),
    },
  };
}

function stubApi({ platforms, screen, polls }) {
  const calls = [];
  const original = { post: api.post, get: api.get };
  api.get = async (path) => {
    calls.push({ method: 'GET', path });
    if (path === '/api/oauth') return { platforms };
    throw new Error(`unexpected GET ${path}`);
  };
  api.post = async (path, body) => {
    calls.push({ method: 'POST', path, body });
    if (path.endsWith('/screen')) return screen;
    if (path.includes('/poll')) {
      const next = polls.shift();
      if (!next) throw new Error('the flow polled more often than the test scripted');
      if (next instanceof Error) throw next;
      return next;
    }
    throw new Error(`unexpected POST ${path}`);
  };
  return {
    calls,
    restore() {
      api.post = original.post;
      api.get = original.get;
    },
  };
}

const cursor = { id: 'cursor-work', auth: { type: 'oauth', platform: 'cursor' } };
const platform = { id: 'cursor', label: 'Cursor', flow: 'screen', flows: ['screen'] };

// The click handler starts the flow and reports its own failures; it deliberately does not
// return the promise, so a test waits for the observable result instead of for the click.
async function clickConnect(form) {
  const button = walk(form).find((n) => n.tagName === 'BUTTON' && textOf(n) === 'Connect');
  assert.ok(button, 'the drawer offers a Connect button');
  for (const fn of button.listeners.click ?? []) await fn({ preventDefault() {} });
  return button;
}

async function until(predicate, what) {
  for (let i = 0; i < 500; i += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error(`timed out waiting for ${what}`);
}

const settled = (ctx) => () => ctx.successes.length > 0 || ctx.errors.length > 0;

test('the page flow opens the provider page and learns of completion by polling', async () => {
  opened.length = 0;
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-9', login_url: 'https://cursor.com/loginDeepControl?uuid=u1' },
    polls: [
      { status: 'pending', retry_after_seconds: 1 },
      { status: 'complete', account: { label: 'Cursor account' } },
    ],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    await until(settled(ctx), 'the flow to finish');

    const start = stub.calls.find((c) => c.path === '/api/oauth/cursor/screen');
    assert.deepEqual(start, {
      method: 'POST',
      path: '/api/oauth/cursor/screen',
      body: { provider_id: 'cursor-work' },
    }, 'the start call names the platform in the path and the provider in the body');
    assert.deepEqual(opened, [{
      url: 'https://cursor.com/loginDeepControl?uuid=u1',
      target: '_blank',
    }], 'the provider page is opened, and nothing else is');

    const polls = stub.calls.filter((c) => c.path.includes('/poll'));
    assert.deepEqual(polls.map((c) => c.path), [
      '/api/oauth/sessions/sess-9/poll',
      '/api/oauth/sessions/sess-9/poll',
    ], 'the same session is polled until it completes');
    assert.deepEqual(polls[0].body, { provider_id: 'cursor-work' });
    assert.equal(
      stub.calls.some((c) => c.path.endsWith('/complete')),
      false,
      'nothing comes back to this router, so /complete must never be called',
    );

    assert.deepEqual(ctx.successes, ['Connected Cursor account.']);
    assert.match(textOf(form), /Connected\./);
  } finally {
    stub.restore();
  }
});

test('the reopen link points at the same page, so a lost tab is recoverable', async () => {
  let release;
  const gate = { promise: new Promise((resolve) => { release = resolve; }), resolve: (v) => release(v) };
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-10', login_url: 'https://cursor.com/loginDeepControl?uuid=u2' },
    polls: [gate.promise, { status: 'complete', account: null }],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    // The poll is gated so the waiting state can be observed rather than raced past.
    await until(() => walk(form).some((n) => n.tagName === 'A'), 'the waiting state');
    const link = walk(form).find((n) => n.tagName === 'A' && textOf(n) === 'Reopen the page');
    assert.ok(link, 'the waiting state offers a link back to the page');
    assert.equal(link.getAttribute('href'), 'https://cursor.com/loginDeepControl?uuid=u2');
    assert.equal(link.getAttribute('target'), '_blank');
    gate.resolve({ status: 'pending', retry_after_seconds: 1 });
    await until(settled(ctx), 'the gated flow to finish');
  } finally {
    stub.restore();
  }
});

test('a failed poll reports the server message and stops polling', async () => {
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-11', login_url: 'https://cursor.com/loginDeepControl?uuid=u3' },
    polls: [{ status: 'failed', message: 'the login page expired; start again' }],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    await until(settled(ctx), 'the flow to finish');

    assert.equal(ctx.errors.length, 1, 'the failure is reported once');
    assert.match(String(ctx.errors[0].err?.message ?? ctx.errors[0].err), /expired/);
    assert.match(textOf(form), /the login page expired; start again/);
    assert.equal(
      stub.calls.filter((c) => c.path.includes('/poll')).length,
      1,
      'a failed session is final — polling it again would only repeat the message',
    );
    assert.deepEqual(ctx.successes, []);
  } finally {
    stub.restore();
  }
});

test('a single wire error is retried while the session is still alive', async () => {
  const wireError = Object.assign(new Error('network'), { status: 502 });
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-12', login_url: 'https://cursor.com/loginDeepControl?uuid=u4' },
    polls: [wireError, { status: 'complete', account: { label: 'Cursor account' } }],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    await until(settled(ctx), 'the flow to finish');

    assert.equal(stub.calls.filter((c) => c.path.includes('/poll')).length, 2);
    assert.deepEqual(ctx.successes, ['Connected Cursor account.']);
    assert.equal(ctx.errors.length, 0, `a transient wire error is not shown as a login failure: ${JSON.stringify(ctx.errors.map((e) => String(e.err?.message ?? e.err)))}`);
  } finally {
    stub.restore();
  }
});

// A drawer the operator closed while the provider's page was open must not reopen or navigate.
// That decision is read lazily, because the form outlives the click that created it.
test('completion navigates only while the drawer is still open', async () => {
  for (const stillOpen of [true, false]) {
    const stub = stubApi({
      platforms: [platform],
      screen: { session_id: `sess-nav-${stillOpen}`, login_url: 'https://cursor.com/loginDeepControl?uuid=un' },
      polls: [{ status: 'complete', account: { label: 'Cursor account' } }],
    });
    let navigated = 0;
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, async () => { navigated += 1; }, () => stillOpen);
    try {
      await clickConnect(form);
      await until(settled(ctx), 'the flow to finish');
      assert.deepEqual(ctx.successes, ['Connected Cursor account.']);
      assert.equal(ctx.errors.length, 0, `the flow reports no failure: ${JSON.stringify(ctx.errors.map((e) => String(e.err?.message ?? e.err)))}`);
      assert.equal(navigated, stillOpen ? 1 : 0, stillOpen
        ? 'an open drawer hands off to the refresh so the new account appears'
        : 'a drawer the operator dismissed stays dismissed');
    } finally {
      stub.restore();
    }
  }
});

test('an expired session ends the flow instead of polling a dead id', async () => {
  const gone = Object.assign(new Error('OAuth session not found or expired'), { status: 404 });
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-13', login_url: 'https://cursor.com/loginDeepControl?uuid=u5' },
    polls: [gone],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    await until(settled(ctx), 'the flow to finish');

    assert.equal(stub.calls.filter((c) => c.path.includes('/poll')).length, 1, '404 is final');
    assert.equal(ctx.errors.length, 1);
    assert.deepEqual(ctx.successes, []);
  } finally {
    stub.restore();
  }
});

// The server uses the status to say which failures are worth another poll. A 4xx it will not
// change its mind about (a dead session, a request it refuses) must end the flow; a busy session, a
// rate limit or a server fault must not, or one hiccup throws away a sign-in the operator already
// started in the other tab.
test('a refusal ends the flow while an invitation to retry does not', async () => {
  const pollsOf = (stub) => stub.calls.filter((c) => c.path.includes('/poll')).length;

  for (const status of [400, 403, 410]) {
    const refusal = Object.assign(new Error('OAuth session provider binding does not match'), { status });
    const stub = stubApi({
      platforms: [platform],
      screen: { session_id: `sess-refuse-${status}`, login_url: 'https://cursor.com/loginDeepControl?uuid=rf' },
      polls: [refusal],
    });
    try {
      const ctx = makeCtx();
      const form = buildOAuthAccountForm(cursor, ctx, null);
      await clickConnect(form);
      await until(settled(ctx), 'the flow to finish');
      assert.equal(pollsOf(stub), 1, `${status} is final`);
      assert.equal(ctx.errors.length, 1, `${status} is reported`);
    } finally {
      stub.restore();
    }
  }

  for (const status of [409, 429, 500]) {
    const retryable = Object.assign(new Error('not now'), { status });
    const stub = stubApi({
      platforms: [platform],
      screen: { session_id: `sess-retry-${status}`, login_url: 'https://cursor.com/loginDeepControl?uuid=rt' },
      polls: [retryable, { status: 'complete', account: { label: 'Cursor account' } }],
    });
    try {
      const ctx = makeCtx();
      const form = buildOAuthAccountForm(cursor, ctx, null);
      await clickConnect(form);
      await until(settled(ctx), 'the flow to finish');
      assert.equal(pollsOf(stub), 2, `${status} is retried`);
      assert.deepEqual(ctx.successes, ['Connected Cursor account.'], `${status} does not end the flow`);
    } finally {
      stub.restore();
    }
  }
});

test('a start call that returns no page fails immediately instead of polling', async () => {
  const stub = stubApi({
    platforms: [platform],
    screen: { session_id: 'sess-14', login_url: '' },
    polls: [],
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(cursor, ctx, null);
    await clickConnect(form);
    await until(settled(ctx), 'the flow to finish');

    assert.equal(stub.calls.filter((c) => c.path.includes('/poll')).length, 0);
    assert.equal(ctx.errors.length, 1);
    assert.match(String(ctx.errors[0].err?.message ?? ctx.errors[0].err), /sign-in page/);
  } finally {
    stub.restore();
  }
});

test.after(() => {
  globalThis.setTimeout = realSetTimeout;
});

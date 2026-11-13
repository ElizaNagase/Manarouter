// Guards the provider preset table in panel/view-providers.js.
//
// Two failures have happened here and nothing else catches either:
//
//   * a preset whose `dialect` is not in the panel's DIALECTS list, which turns "Add provider"
//     into a save-time error (the pre-rename `command-code` spelling),
//   * a preset that drifts from catalog/providers.toml, so the panel offers an endpoint the
//     compiled catalog does not describe.
//
// DIALECTS itself is pinned to the Rust serde form by the cross-language test
// `the_panel_offers_only_dialects_this_binary_deserialises` in mr-core, so this file only has to
// keep the presets inside that list.
//
// The preset table is a pure data literal, so it is read out of the source and evaluated rather
// than imported: view-providers.js pulls in the DOM layer at module scope and cannot load in Node.
//
// Run: node --test panel/test/provider-presets.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const here = dirname(fileURLToPath(import.meta.url));
const panel = join(here, '..');
const repo = join(panel, '..');

const source = readFileSync(join(panel, 'view-providers.js'), 'utf8');

/// Slice out the balanced `{...}` or `[...]` that follows `opener`, skipping string literals.
function literal(text, opener) {
  const start = text.indexOf(opener);
  assert.notEqual(start, -1, `${opener} not found in view-providers.js`);
  const open = opener[opener.length - 1];
  const close = open === '{' ? '}' : ']';
  let depth = 0;
  for (let i = start + opener.length - 1; i < text.length; i += 1) {
    const ch = text[i];
    if (ch === "'" || ch === '"' || ch === '`') {
      const quote = ch;
      i += 1;
      while (i < text.length && text[i] !== quote) {
        if (text[i] === '\\') i += 1;
        i += 1;
      }
      continue;
    }
    if (ch === open) depth += 1;
    else if (ch === close) {
      depth -= 1;
      if (depth === 0) return text.slice(start + opener.length - 1, i + 1);
    }
  }
  throw new Error(`unterminated literal after ${opener}`);
}

/// Drop whole-line `//` comments so an apostrophe in prose cannot confuse the scanner.
function withoutLineComments(text) {
  return text
    .split('\n')
    .filter((line) => !line.trimStart().startsWith('//'))
    .join('\n');
}

const presets = vm.runInNewContext(
  `(${withoutLineComments(literal(source, 'const PROVIDER_PRESETS = {'))})`,
);
const dialects = vm.runInNewContext(literal(source, 'const DIALECTS = ['));
const authKinds = vm
  .runInNewContext(literal(source, 'const AUTH_KINDS = ['))
  .map(([kind]) => kind);

const all = [...presets.providers, ...presets.platforms];
// The panel fills the dialect's login in from the presets, so a preset missing its `platform`
// silently produces a provider that can never be connected. The panel's own dialect map is
// derived from this table; the assertion below pins that it stays derived rather than becoming a
// second hand-kept copy.
const oauthPlatforms = Object.fromEntries(
  presets.platforms
    .filter((preset) => preset.auth === 'oauth' && preset.platform)
    .map((preset) => [preset.dialect, preset.platform]),
);

test('every preset dialect is one the panel offers', () => {
  for (const preset of all) {
    assert.ok(
      dialects.includes(preset.dialect),
      `${preset.name} uses dialect ${preset.dialect}, which DIALECTS does not list`,
    );
  }
});

test('every preset is complete and its auth kind is offered', () => {
  for (const preset of all) {
    for (const key of ['name', 'dialect', 'baseUrl', 'auth', 'hint']) {
      assert.equal(typeof preset[key], 'string', `${preset.name} is missing ${key}`);
    }
    assert.ok(preset.name.length > 0, 'a preset has an empty name');
    assert.ok(preset.hint.length > 0, `${preset.name} has no hint`);
    assert.ok(authKinds.includes(preset.auth), `${preset.name} uses unknown auth ${preset.auth}`);
  }
});

test('every OAuth preset names the platform its dialect signs in through', () => {
  const oauth = all.filter((preset) => preset.auth === 'oauth');
  assert.ok(oauth.length >= 4, 'the OAuth presets went missing');
  for (const preset of oauth) {
    assert.equal(
      typeof preset.platform,
      'string',
      `${preset.name} is an OAuth preset with no platform, so Connect would have no login to start`,
    );
    assert.match(preset.platform, /^[a-z][a-z0-9-]*$/, `${preset.name} has a malformed platform id`);
    assert.equal(
      oauthPlatforms[preset.dialect],
      preset.platform,
      `dialect ${preset.dialect} maps to platform ${oauthPlatforms[preset.dialect]}, but ${preset.name} names ${preset.platform}`,
    );
  }
  assert.match(
    source,
    /const DIALECT_OAUTH_PLATFORMS = Object\.freeze\(Object\.fromEntries\(\s*PROVIDER_PRESETS\.platforms/,
    'DIALECT_OAUTH_PLATFORMS must be derived from the presets, not written out a second time',
  );
  // The map exists to decide the auth kind when the dialect is chosen, so a dialect it lists as
  // OAuth-only must not also be offered with a bearer path by some other preset.
  for (const dialect of Object.keys(oauthPlatforms)) {
    const bearer = all.filter((preset) => preset.dialect === dialect && preset.auth !== 'oauth');
    assert.deepEqual(bearer.map((preset) => preset.name), [], `dialect ${dialect} has a non-OAuth preset`);
  }
});

test('each OAuth preset platform is the one the catalog signs in with', () => {
  const toml = readFileSync(join(repo, 'catalog/providers.toml'), 'utf8');
  const byDialect = new Map();
  for (const block of toml.split('[[providers]]').slice(1)) {
    const dialect = block.match(/^dialect = "([^"]*)"/m);
    const auth = block.match(/^auth = \{ type = "oauth", platform = "([^"]*)" \}/m);
    if (dialect && auth) byDialect.set(dialect[1], auth[1]);
  }
  for (const preset of all.filter((item) => item.auth === 'oauth')) {
    assert.equal(
      byDialect.get(preset.dialect),
      preset.platform,
      `catalog/providers.toml gives dialect ${preset.dialect} platform ${byDialect.get(preset.dialect)}, but the panel names ${preset.platform}`,
    );
  }
});

test('preset names are unique across both groups', () => {
  const seen = new Set();
  for (const preset of all) {
    assert.ok(!seen.has(preset.name), `duplicate preset name ${preset.name}`);
    seen.add(preset.name);
  }
});

test('Modal vLLM is offered exactly once with its dedicated dialect and auth', () => {
  const modal = all.filter((preset) => preset.name === 'Modal vLLM');
  assert.equal(modal.length, 1);
  assert.equal(modal[0].dialect, 'modal-vllm');
  assert.equal(modal[0].auth, 'modal');
  assert.match(modal[0].hint, /HTTPS public URL/i);
  assert.match(modal[0].hint, /Token ID\.Token Secret/i);
});

test('the OpenCode presets point at the documented hosts', () => {
  const byName = new Map(all.map((preset) => [preset.name, preset]));
  const zen = byName.get('OpenCode Zen');
  const go = byName.get('OpenCode Go');
  assert.ok(zen, 'OpenCode Zen preset is missing');
  assert.ok(go, 'OpenCode Go preset is missing');
  assert.equal(zen.baseUrl, 'https://opencode.ai/zen/v1');
  assert.equal(go.baseUrl, 'https://opencode.ai/zen/go/v1');
  for (const preset of [zen, go]) {
    assert.equal(preset.dialect, 'open-ai');
    assert.equal(preset.auth, 'bearer');
  }
});

test('a preset that names a catalog provider agrees with the catalog', () => {
  const toml = readFileSync(join(repo, 'catalog/providers.toml'), 'utf8');
  const catalog = new Map();
  for (const block of toml.split('[[providers]]').slice(1)) {
    const label = block.match(/^label = "([^"]*)"/m);
    const baseUrl = block.match(/^base_url = "([^"]*)"/m);
    if (label && baseUrl) catalog.set(label[1], baseUrl[1]);
  }
  // The catalog records the prefix a dialect appends its route path to, and the panel records
  // what a person types. They may differ by a trailing `/v1` (DeepSeek documents both), so the
  // check is that one is a path prefix of the other rather than that they are identical.
  const agrees = (a, b) => a === b || a.startsWith(`${b}/`) || b.startsWith(`${a}/`);
  for (const preset of all) {
    const documented = catalog.get(preset.name);
    if (documented === undefined) continue;
    assert.ok(
      agrees(preset.baseUrl, documented),
      `${preset.name} is ${preset.baseUrl} in the panel but ${documented} in catalog/providers.toml`,
    );
  }
});

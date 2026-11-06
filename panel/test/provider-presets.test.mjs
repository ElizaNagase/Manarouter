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

test('preset names are unique across both groups', () => {
  const seen = new Set();
  for (const preset of all) {
    assert.ok(!seen.has(preset.name), `duplicate preset name ${preset.name}`);
    seen.add(preset.name);
  }
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

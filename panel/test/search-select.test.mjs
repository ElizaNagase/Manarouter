// The suggestion list behind the combo provider/model pickers.
//
// The point of this component over a native <datalist> is that the page owns the order and the
// filtering: alphabetical by default, narrowing on every keystroke. That decision is a pure
// function, so it is asserted here without a browser.
//
// Run: node --test panel/test/search-select.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { filterOptions, sortOptions } from '../search-select.js';

const provider = (id, label) => ({ value: id, label: label ?? id });

test('providers and models are alphabetical by default, not in config order', () => {
  const options = [provider('openrouter'), provider('command-code'), provider('deepseek')];
  assert.deepEqual(sortOptions(options).map((o) => o.value), ['command-code', 'deepseek', 'openrouter']);
});

test('the order is case-insensitive so a lowercase id does not sort after every capital', () => {
  const options = [provider('zeta'), provider('Alpha'), provider('beta')];
  assert.deepEqual(sortOptions(options).map((o) => o.value), ['Alpha', 'beta', 'zeta']);
});

test('typing one letter narrows the list on that keystroke', () => {
  const options = [provider('openai-codex'), provider('opencode-go'), provider('deepseek')];
  assert.deepEqual(filterOptions(options, 'o').map((o) => o.value), ['openai-codex', 'opencode-go']);
  assert.deepEqual(filterOptions(options, 'op').map((o) => o.value), ['openai-codex', 'opencode-go']);
  assert.deepEqual(filterOptions(options, 'opencode').map((o) => o.value), ['opencode-go']);
});

test('the query also matches the label and the hint, not only the id', () => {
  const options = [
    { value: 'deepseek', label: 'DeepSeek', hint: 'china' },
    { value: 'local_orchestrator', label: 'local', hint: 'llama' },
  ];
  assert.deepEqual(filterOptions(options, 'seek').map((o) => o.value), ['deepseek']);
  assert.deepEqual(filterOptions(options, 'llama').map((o) => o.value), ['local_orchestrator']);
});

test('filtering keeps the alphabetical order instead of ranking matches by position', () => {
  const options = [provider('zz-deepseek'), provider('aa-deepseek')];
  assert.deepEqual(filterOptions(options, 'deepseek').map((o) => o.value), ['aa-deepseek', 'zz-deepseek']);
});

test('a query with no match is an empty list, and an empty query is every option', () => {
  const options = [provider('a'), provider('b')];
  assert.deepEqual(filterOptions(options, 'nothing'), []);
  assert.deepEqual(filterOptions(options, '  ').map((o) => o.value), ['a', 'b']);
  assert.deepEqual(filterOptions(null, 'a'), []);
});

test('a long catalogue is capped without changing which entries come first', () => {
  const options = Array.from({ length: 30 }, (_, i) => provider('p' + String(i).padStart(2, '0')));
  const shown = filterOptions(options, '', { limit: 10 });
  assert.equal(shown.length, 10);
  assert.deepEqual(shown.map((o) => o.value), options.slice(0, 10).map((o) => o.value));
});

test('sorting never mutates the caller array', () => {
  const options = [provider('b'), provider('a')];
  sortOptions(options);
  assert.deepEqual(options.map((o) => o.value), ['b', 'a']);
});

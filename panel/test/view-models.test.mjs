// The models list is sorted model-major: every provider that offers the same model id sits
// together, so an operator can compare availability without hunting down the table. The provider
// is the tie-break inside a model group.
//
// Run: node --test panel/test/view-models.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { compareModelRows, pageBounds } from '../view-models.js';

const row = (provider, model) => ({ p: { id: provider }, m: { id: model } });
const shape = (rows) => rows.map((row) => `${row.m.id}@${row.p.id}`);

test('models are grouped before providers', () => {
  const rows = [
    row('openai', 'gpt-4o'),
    row('anthropic', 'claude-4'),
    row('azure', 'gpt-4o'),
  ];
  rows.sort(compareModelRows);
  assert.deepEqual(shape(rows), [
    'claude-4@anthropic',
    'gpt-4o@azure',
    'gpt-4o@openai',
  ]);
});

test('case variants of one model id are grouped by exact spelling', () => {
  // A case-insensitive comparison alone reports "equal" for GPT-4 and gpt-4. Sorting then falls
  // straight through to the provider, which can interleave the two spellings: a/GPT, b/gpt,
  // c/gpt, d/GPT. The exact-spelling tie-break keeps each exact model id contiguous.
  const rows = [
    row('a-provider', 'GPT-4'),
    row('b-provider', 'gpt-4'),
    row('c-provider', 'gpt-4'),
    row('d-provider', 'GPT-4'),
  ];
  rows.sort(compareModelRows);
  // Which case sorts first depends on the locale's collation; what matters is that neither exact
  // spelling is split around the other.
  assert.deepEqual(shape(rows), [
    'gpt-4@b-provider',
    'gpt-4@c-provider',
    'GPT-4@a-provider',
    'GPT-4@d-provider',
  ]);
});

test('model ids sort naturally, so model-2 precedes model-10', () => {
  const rows = [row('p', 'model-10'), row('p', 'model-2'), row('p', 'model-1')];
  rows.sort(compareModelRows);
  assert.deepEqual(shape(rows), ['model-1@p', 'model-2@p', 'model-10@p']);
});

test('provider ids use the same natural order within a model group', () => {
  const rows = [row('provider-10', 'same'), row('provider-2', 'same'), row('Provider-1', 'same')];
  rows.sort(compareModelRows);
  assert.deepEqual(shape(rows), [
    'same@Provider-1',
    'same@provider-2',
    'same@provider-10',
  ]);
});

test('a page is clamped to the rows that exist', () => {
  assert.deepEqual(pageBounds(10, 1, 4), { page: 1, pages: 3, start: 0, end: 4 });
  assert.deepEqual(pageBounds(10, 3, 4), { page: 3, pages: 3, start: 8, end: 10 });
  assert.deepEqual(pageBounds(10, 99, 4), { page: 3, pages: 3, start: 8, end: 10 });
  assert.deepEqual(pageBounds(10, 0, 4), { page: 1, pages: 3, start: 0, end: 4 });
});

test('a page size that is zero, negative or not a number still yields one row', () => {
  for (const size of [0, -5, NaN, 'nope', undefined]) {
    assert.deepEqual(pageBounds(3, 1, size), { page: 1, pages: 3, start: 0, end: 1 });
  }
});

test('an empty list still has one page with an empty range', () => {
  assert.deepEqual(pageBounds(0, 7, 50), { page: 1, pages: 1, start: 0, end: 0 });
});

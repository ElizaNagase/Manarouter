// Reasoning-level views for the panel's pickers.
//
// The backend annotates each provider with `dialect_reasoning_levels` (what its dialect can put on
// the wire) and each model with `effective_reasoning_levels` (what the compiler will actually
// offer). These helpers turn that into the lists the pickers show, so the panel cannot advertise a
// tier the validator refuses or the compiler silently lowers.
//
// Run: node --test panel/test/reasoning-levels.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import {
  providerLevels,
  declaredLevels,
  modelLevels,
  comboLevels,
  stepModel,
  stepProvider,
} from '../format.js';

const openAiLevels = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh'];
const budgetLevels = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'ultra', 'max'];

function provider(overrides = {}) {
  return {
    id: 'p1',
    dialect: 'open-ai',
    dialect_reasoning_levels: openAiLevels,
    models: [],
    ...overrides,
  };
}

test('a_provider_picker_offers_the_levels_the_dialect_can_send', () => {
  assert.deepEqual(providerLevels(provider()), openAiLevels);
  assert.deepEqual(
    providerLevels(provider({ dialect: 'anthropic', dialect_reasoning_levels: budgetLevels })),
    budgetLevels,
  );
});

test('the_dialect_list_is_read_from_the_model_entry_the_control_plane_annotates', () => {
  // The control plane annotates model entries, not the provider object, because an unknown
  // provider-level key would survive a round trip through the config's flatten catch-all.
  const model = { id: 'm1', dialect_reasoning_levels: ['none', 'low', 'high'] };
  assert.deepEqual(providerLevels({ id: 'p1' }, model), ['none', 'low', 'high']);
  assert.deepEqual(modelLevels({ id: 'm1' }, { id: 'p1' }), [
    'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'ultra', 'max',
  ]);
});

test('a_response_without_the_list_falls_back_to_the_whole_ladder', () => {
  // An older control plane does not annotate providers; the picker must still work rather than
  // render an empty list.
  assert.deepEqual(providerLevels({ id: 'p1' }), [
    'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'ultra', 'max',
  ]);
});

test('declared_levels_keep_only_canonical_spellings_in_ascending_order', () => {
  assert.deepEqual(declaredLevels({ reasoning_levels: ['high', 'telepathy', 'low'] }), ['low', 'high']);
  assert.deepEqual(declaredLevels({ reasoning_levels: [] }), []);
  assert.deepEqual(declaredLevels({}), []);
});

test('the_models_effective_levels_win_over_its_declared_list', () => {
  // The compiler capped the catalogue's `max` to the dialect's vocabulary, so the picker follows
  // the compiler, not the stale declaration.
  const model = { reasoning_levels: ['low', 'max'], effective_reasoning_levels: ['low', 'high'] };
  assert.deepEqual(modelLevels(model, provider()), ['low', 'high']);
});

test('a_model_that_offers_no_levels_offers_none_rather_than_the_provider_list', () => {
  // An empty effective list is an answer: a plain chat model has no reasoning tiers. Falling
  // through to the provider vocabulary would advertise levels the compiler holds empty.
  assert.deepEqual(modelLevels({ effective_reasoning_levels: [] }, provider()), []);
});

test('a_model_without_the_annotation_keeps_the_old_declared_then_vocabulary_order', () => {
  assert.deepEqual(modelLevels({ reasoning_levels: ['high', 'low'] }, provider()), ['low', 'high']);
  assert.deepEqual(modelLevels({}, provider()), openAiLevels);
});

test('a_combo_level_list_includes_levels_the_model_only_derives', () => {
  // Nothing is declared on the entry, but the provider dialect supplies the ladder, and the relay
  // really resolves `coding-high`, so the summary must name it.
  const models = [{ id: 'm1', effective_reasoning_levels: ['low', 'high'] }];
  const providers = [provider({ models })];
  const steps = [{ provider: 'p1', model: 'm1' }];
  assert.deepEqual(comboLevels(steps, providers), ['low', 'high']);
});

test('a_combo_level_list_is_a_union_so_every_step_contributes_its_ceiling', () => {
  const models = [
    { id: 'm1', effective_reasoning_levels: ['low', 'high'] },
    { id: 'm2', effective_reasoning_levels: ['medium', 'max'] },
  ];
  const steps = [
    { provider: 'p1', model: 'm1' },
    { provider: 'p1', model: 'm2' },
  ];
  assert.deepEqual(comboLevels(steps, [provider({ models })]), ['low', 'medium', 'high', 'max']);
});

test('a_combo_whose_models_offer_nothing_lists_no_level_names', () => {
  const models = [{ id: 'm1', effective_reasoning_levels: [] }];
  assert.deepEqual(comboLevels([{ provider: 'p1', model: 'm1' }], [provider({ models })]), []);
});

test('a_step_resolves_its_provider_even_when_the_model_entry_is_missing', () => {
  const providers = [provider()];
  const step = { provider: 'p1', model: 'gone' };
  assert.equal(stepProvider(step, providers)?.id, 'p1');
  assert.equal(stepModel(step, providers), null);
});

// view-models.js — every model across every provider, filterable by provider and source,
// with hide/show toggles. Source chips are neutral grey on purpose: synced/manual/inferred is
// provenance, not health — semantic colours stay semantic.

import { api } from './api.js';
import { h, chip, emptyState, errorBlock, field, toggle, copyButton } from './dom.js';
import { formatCompact, REASONING_LEVELS, providerLevels, declaredLevels, modelLevels } from './format.js';

const SOURCES = ['synced', 'manual', 'inferred'];
const INPUT_MODALITIES = ['text', 'image', 'audio', 'video', 'pdf', 'file'];
const MODEL_TEST_PROMPT = 'reply with "yes"';

export async function renderModels(mount, ctx) {
  let providers = [];

  const provSel = h('select', { class: 'input', 'aria-label': 'Filter by provider' });
  const sourceSel = h('select', { class: 'input', 'aria-label': 'Filter by source' },
    h('option', { value: '' }, 'all sources'),
    ...SOURCES.map((s) => h('option', { value: s }, s)));
  const search = h('input', { class: 'input', type: 'search', 'aria-label': 'Search model id or label', placeholder: 'search…' });
  const hiddenFilter = toggle('hidden only', {
    id: 'models-hidden-only',
    help: 'Show only models currently hidden from the relay listing.',
  });
  const hiddenOnly = hiddenFilter.input;
  const tableWrap = h('div', { class: 'table-scroll' });
  const count = h('span', { class: 'count' });

  // A <select> fires both "input" and "change"; binding both rebuilt the table twice per change.
  provSel.addEventListener('change', renderTable);
  sourceSel.addEventListener('change', renderTable);
  search.addEventListener('input', renderTable);
  hiddenOnly.addEventListener('change', renderTable);

  mount.replaceChildren(
    h('div', { class: 'toolbar' },
      provSel, sourceSel, search,
      hiddenFilter.row,
      h('span', { class: 'spacer' }), count),
    tableWrap,
  );

  await reload();

  async function reload() {
    try {
      providers = await ctx.loadProviders(true);
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    const previousProvider = provSel.value;
    provSel.replaceChildren(
      h('option', { value: '' }, 'all providers'),
      ...providers.map((p) => h('option', { value: p.id }, p.id)),
    );
    // replaceChildren() resets a <select> to its first option; keep the operator's filter.
    if (previousProvider && providers.some((p) => p.id === previousProvider)) provSel.value = previousProvider;
    renderTable();
  }

  function rows() {
    const q = search.value.trim().toLowerCase();
    const out = [];
    for (const p of providers) {
      if (provSel.value && p.id !== provSel.value) continue;
      for (const m of p.models ?? []) {
        if (sourceSel.value && (m.source ?? 'synced') !== sourceSel.value) continue;
        if (hiddenOnly.checked && !m.hidden) continue;
        if (q && !`${m.id} ${m.label ?? ''}`.toLowerCase().includes(q)) continue;
        out.push({ p, m });
      }
    }
    return out;
  }

  function renderTable() {
    const list = rows();
    const totalModels = providers.reduce((n, p) => n + (p.models?.length ?? 0), 0);
    const hiddenCount = providers.reduce((n, p) => n + (p.models ?? []).filter((m) => m.hidden).length, 0);
    count.textContent = `${list.length} shown · ${totalModels} across ${providers.length} providers · ${hiddenCount} hidden`;

    if (!totalModels) {
      tableWrap.replaceChildren(emptyState('No models known.',
        'Models appear here after a provider sync, or when you add one manually in the provider detail.'));
      return;
    }
    if (!list.length) {
      tableWrap.replaceChildren(emptyState('Nothing matches these filters.'));
      return;
    }

    const tbody = h('tbody');
    for (const { p, m } of list) {
      const visibility = toggle('Hidden from relay', {
        checked: Boolean(m.hidden),
        help: 'Hidden models are excluded from the relay model listing.',
        onChange: (event) => toggleHidden(p, m, event.currentTarget),
      });
      const test = h('button', {
        class: 'btn btn-primary btn-sm', type: 'button',
        'aria-label': `Test ${m.id} on provider ${p.id}`,
        title: 'May consume provider quota. Sends the fixed prompt: reply with "yes".',
      }, 'Test');
      test.addEventListener('click', () => testModel(p, m, test));
      const metadata = modelMetadataEditor(p, m);
      const del = h('button', {
        class: 'btn btn-danger btn-sm', type: 'button',
        'aria-label': `Delete ${m.id} on provider ${p.id}`,
      }, 'Delete');
      del.addEventListener('click', () => deleteModel(p, m));

      tbody.append(h('tr', {},
        h('td', { class: 'mono' }, p.id),
        h('td', { class: 'mono' }, m.id),
        h('td', {}, m.label ?? ''),
        h('td', {}, chip(m.source ?? 'synced', 'chip-neutral', 'provenance')),
        h('td', { class: 'num' }, m.context_length ? formatCompact(m.context_length) : ''),
        h('td', {}, priceText(m)),
        h('td', {}, metadata),
        h('td', {}, m.hidden ? chip('hidden', 'chip-neutral', 'excluded from the relay\u2019s model listing') : null),
        h('td', { class: 'actions' }, test, visibility.row, del)));
    }
    tableWrap.replaceChildren(
      h('p', { class: 'hint', role: 'note' }, 'Model tests send the fixed prompt reply with "yes" only after you click Test. Requests may consume provider quota.'),
      h('table', { class: 'tbl' },
        h('thead', {}, h('tr', {},
          h('th', { scope: 'col' }, 'Provider'),
          h('th', { scope: 'col' }, 'Model'),
          h('th', { scope: 'col' }, 'Label'),
          h('th', { scope: 'col' }, 'Source'),
          h('th', { scope: 'col' }, 'Ctx'),
          h('th', { scope: 'col' }, '$/1M in · out'),
          h('th', { scope: 'col' }, 'Reasoning · input'),
          h('th', { scope: 'col' }, ''),
          h('th', { scope: 'col' }, ''))),
        tbody));
  }

  function priceText(m) {
    const input = m.pricing?.input_per_mtok ?? m.input_price_per_mtok;
    const output = m.pricing?.output_per_mtok ?? m.output_price_per_mtok;
    const rate = (value) => typeof value === 'number' && Number.isFinite(value) ? `$${formatCompact(value)}` : '—';
    if (input == null && output == null) return '—';
    return `${rate(input)} · ${rate(output)}`;
  }

  function modelMetadataEditor(p, m) {
    const dialectLevels = providerLevels(p, m);
    const configuredLevels = declaredLevels(m);
    const effectiveLevels = modelLevels(m, p);
    const levelsDerived = configuredLevels.length === 0;
    const modalitiesDerived = !Array.isArray(m.input_modalities) || m.input_modalities.length === 0;
    const configuredModalities = Array.isArray(m.input_modalities) ? m.input_modalities.filter((modality) => INPUT_MODALITIES.includes(modality)) : [];
    const defaultLevel = REASONING_LEVELS.includes(m.default_reasoning_level) ? m.default_reasoning_level : '';
    const defaultLevelLabel = defaultLevel || 'inherit global';
    const effectiveDefault = REASONING_LEVELS.includes(m.effective_default_reasoning_level)
      ? m.effective_default_reasoning_level
      : '';
    // The picker offers the provider's levels, never the whole ladder: a level its dialect cannot
    // send would be refused by the backend on save. A stored default outside that list is kept as
    // an option so opening the editor does not silently rewrite the value.
    const defaultChoices = !defaultLevel || dialectLevels.includes(defaultLevel)
      ? dialectLevels
      : [...dialectLevels, defaultLevel];
    // A level the entry declares but the dialect cannot send is not offered (the backend would
    // refuse the save), and it is named in the hint instead of disappearing quietly on the next
    // save.
    const unsendableLevels = configuredLevels.filter((level) => !dialectLevels.includes(level));
    const unsendableNote = unsendableLevels.length
      ? ` Declared but unsendable on this provider: ${unsendableLevels.join(', ')} — saving will drop it.`
      : '';
    // Validation refuses a default the declared list does not contain, so say so before the save
    // turns into a 400 with the same words.
    const strayDefaultNote = defaultLevel && configuredLevels.length && !configuredLevels.includes(defaultLevel)
      ? ` The default ${defaultLevel} is not one of the declared levels; check it or clear it to inherit.`
      : '';
    const levels = new Set(configuredLevels);
    const modalities = new Set(configuredModalities);
    const levelInputs = [];
    const modalityInputs = [];
    const defaultSelect = h('select', { class: 'input', 'aria-label': `Default reasoning level for ${m.id}` },
      h('option', { value: '' }, 'inherit global default'),
      ...defaultChoices.map((level) => h('option', { value: level }, level)));
    defaultSelect.value = defaultLevel;
    const save = h('button', { class: 'btn btn-sm', type: 'button' }, 'Save metadata');
    const editor = h('details', { class: 'model-metadata' },
      h('summary', {}, 'Configure'),
      h('p', { class: 'hint' }, `This ${p.dialect} provider can send: ${dialectLevels.join(', ')}. The model currently offers: ${effectiveLevels.length ? effectiveLevels.join(', ') : 'no reasoning levels'}${levelsDerived ? ' (derived; nothing is declared on this entry)' : ''}.${unsendableNote}${strayDefaultNote} Runs at ${effectiveDefault || defaultLevelLabel} when the name carries no level · Input modalities: ${modalitiesDerived ? 'automatic (derived by server)' : configuredModalities.join(', ')}. Saving writes the checked list explicitly.`),
      h('span', { class: 'field-label' }, 'Supported reasoning levels'),
      h('div', { class: 'model-meta-options' }, ...dialectLevels.map((level) => {
        const control = toggle(level, { checked: levels.has(level), id: `model-${p.id}-${m.id}-${level}` });
        control.input.value = level;
        control.input.addEventListener('change', () => control.input.checked ? levels.add(level) : levels.delete(level));
        levelInputs.push(control.input);
        return control.row;
      })),
      field('Per-model default reasoning level', defaultSelect, { hint: 'Choose a level this provider can send, or inherit the global default.' }),
      h('span', { class: 'field-label' }, 'Accepted input modalities'),
      h('div', { class: 'model-meta-options' }, ...INPUT_MODALITIES.map((modality) => {
        const control = toggle(modality, { checked: modalities.has(modality), id: `model-${p.id}-${m.id}-input-${modality}` });
        control.input.value = modality;
        control.input.addEventListener('change', () => control.input.checked ? modalities.add(modality) : modalities.delete(modality));
        modalityInputs.push(control.input);
        return control.row;
      })),
      save);
    save.addEventListener('click', async () => {
      const nextLevels = levelInputs.filter((input) => input.checked).map((input) => input.value);
      const nextModalities = modalityInputs.filter((input) => input.checked).map((input) => input.value);
      if (nextLevels.some((level) => !REASONING_LEVELS.includes(level))
        || nextModalities.some((modality) => !INPUT_MODALITIES.includes(modality))
        || (defaultSelect.value && !REASONING_LEVELS.includes(defaultSelect.value))) {
        ctx.toast.error('Metadata was not saved: choose only listed reasoning levels and input modalities.');
        return;
      }
      save.disabled = true;
      try {
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, {
          ...m,
          reasoning_levels: nextLevels,
          default_reasoning_level: defaultSelect.value || null,
          input_modalities: nextModalities,
        });
        ctx.toast.success(`Metadata saved for ${m.id}.`);
        await reload();
      } catch (err) {
        save.disabled = false;
        ctx.toast.error('Could not save model metadata', err);
      }
    });
    return editor;
  }

  async function testModel(p, m, button) {
    button.disabled = true;
    const originalText = button.textContent;
    button.textContent = 'Testing…';
    try {
      const started = performance.now();
      // `skipAuthRedirect`: a provider that refuses the test with its own 401 is not a dead panel
      // session — signing in again cannot fix a rejected credential, and the bounce would hide the
      // models list along with the verdict.
      const result = await api.post('/api/models/test', {
        provider: p.id,
        model: m.id,
        prompt: MODEL_TEST_PROMPT,
      }, { skipAuthRedirect: true });
      const elapsed = Math.round(performance.now() - started);
      const reply = result?.response ?? result?.reply ?? result?.output ?? result?.message;
      if (reply === undefined || reply === null) {
        ctx.toast.success(`Test completed in ${elapsed} ms; API returned no reply text. ${JSON.stringify(result)}`);
      } else {
        ctx.toast.success(`Model replied in ${elapsed} ms: ${typeof reply === 'string' ? reply : JSON.stringify(reply)}`);
      }
    } catch (err) {
      const status = Number.isFinite(err?.status) ? `HTTP ${err.status}` : 'API error';
      const code = err?.code ? ` (${err.code})` : '';
      const details = err?.details?.length ? ` — ${err.details.map((item) => typeof item === 'string' ? item : JSON.stringify(item)).join('; ')}` : '';
      ctx.toast.error(`Model test failed: ${status}${code}: ${err?.message ?? 'request failed'}${details}`);
    } finally {
      button.disabled = false;
      button.textContent = originalText;
    }
  }

  async function toggleHidden(p, m, btn) {
    btn.disabled = true;
    try {
      // POST /api/providers/{id}/models is the upsert path; `hidden` is authored metadata and
      // manual entries win over a later sync (docs/03-configuration.md). Assumption documented
      // in panel/README.md — if the backend adds a dedicated toggle route, change it here.
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...m, hidden: !m.hidden });
      await reload();
    } catch (err) {
      btn.checked = Boolean(m.hidden); // reverted: the POST failed, so the server still has the old value
      btn.disabled = false;
      ctx.toast.error('Could not change visibility', err);
    }
  }

  async function deleteModel(p, m) {
    const ok = await ctx.confirm({
      title: `Delete model “${m.id}”?`,
      message: m.source === 'manual'
        ? 'It was entered manually and will not come back.'
        : 'A later sync may re-add it.',
      confirmLabel: 'Delete model',
    });
    if (!ok) return;
    try {
      await api.del(`/api/providers/${encodeURIComponent(p.id)}/models/${encodeURIComponent(m.id)}`);
      ctx.toast.success(`Model ${m.id} deleted.`);
      await reload();
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }
}

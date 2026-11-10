// view-models.js — every model across every provider, filterable by provider and source.
// Most synced models carry no label, context length or price. The page must show the coverage
// honestly rather than leaving columns that are empty for every row, so column visibility is
// computed from the rows on screen. Source chips stay neutral: provenance is not health.

import { api } from './api.js';
import { h, chip, emptyState, errorBlock, field, toggle, copyButton, icon } from './dom.js';
import {
  formatCompact, formatAge, extractList, REASONING_LEVELS, providerLevels, declaredLevels, modelLevels,
} from './format.js';

const SOURCES = ['synced', 'manual', 'inferred'];
const INPUT_MODALITIES = ['text', 'image', 'audio', 'video', 'pdf', 'file'];
const MODEL_TEST_PROMPT = 'reply with "yes"';
const NUMBER = new Intl.NumberFormat();

/** Tiny concatenation helper: this file stays plain ES2019 and avoids template literals so it is
 *  easy to grep and to paste into older tooling. */
function txt(...parts) {
  return parts.join('');
}

/** Compare two ids case-insensitively, with exact spelling as the tie-break so case variants stay
 *  together instead of being split by the next sort key. */
function compareId(a, b) {
  const folded = String(a).localeCompare(String(b), undefined, { sensitivity: 'base', numeric: true });
  if (folded !== 0) return folded;
  return String(a).localeCompare(String(b), { numeric: true });
}

/** Model-major row order: all providers of one model are contiguous, then provider order. */
export function compareModelRows(a, b) {
  return compareId(a.m.id, b.m.id) || compareId(a.p.id, b.p.id);
}

/** Clamp a one-based page to the rows that exist and return the half-open slice for it. Pages are
 *  clamped rather than rejected: hiding a model on the last page must leave the operator on that
 *  page, not on an empty one. */
export function pageBounds(total, page, pageSize) {
  const size = Math.max(1, Math.floor(pageSize) || 1);
  const pages = Math.max(1, Math.ceil(total / size));
  const current = Math.min(Math.max(1, Math.floor(page) || 1), pages);
  const start = (current - 1) * size;
  return { page: current, pages, start, end: Math.min(start + size, total) };
}

const fmt = (value) => NUMBER.format(value);

function labelText(m) {
  return typeof m?.label === 'string' ? m.label.trim() : '';
}

function positiveNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : null;
}

function nonNegativeNumber(value) {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
}

function contextLength(m) {
  return positiveNumber(m?.context_length);
}

function maxOutput(m) {
  return positiveNumber(m?.max_output_tokens);
}

/** Prices have appeared in two shapes over the life of the API; accept either, and only accept real
 *  numbers. A null is "not reported", which is different from a reported price of zero. */
function priceParts(m) {
  const input = nonNegativeNumber(m?.pricing?.input_per_mtok ?? m?.input_price_per_mtok);
  const output = nonNegativeNumber(m?.pricing?.output_per_mtok ?? m?.output_price_per_mtok);
  return { input, output };
}

function hasPrice(m) {
  const { input, output } = priceParts(m);
  return input !== null || output !== null;
}

function priceText(m) {
  const { input, output } = priceParts(m);
  if (input === null && output === null) return '';
  const rate = (value) => value === null ? 'n/r' : '$' + formatCompact(value);
  return txt(rate(input), ' · ', rate(output));
}

function priceTitle(m) {
  const { input, output } = priceParts(m);
  if (input === null && output === null) return 'This provider reports no pricing for this model.';
  const rate = (value) => value === null ? 'not reported' : '$' + fmt(value) + ' per 1M tokens';
  return txt('Input: ', rate(input), ' · Output: ', rate(output));
}

function modalities(m) {
  if (!Array.isArray(m?.input_modalities)) return [];
  return m.input_modalities.filter((modality) => INPUT_MODALITIES.includes(modality));
}

/** A readable name from the id, for the label slot only. The id never moves out of the model
 *  column, so an imperfect derivation cannot make the row ambiguous. */
function deriveDisplayName(id) {
  const raw = typeof id === 'string' ? id.trim() : '';
  if (!raw) return '';
  const tail = raw.slice(raw.lastIndexOf('/') + 1) || raw;
  const words = tail.split(/[._-]+/).filter(Boolean);
  if (!words.length) return '';
  const acronyms = new Set(['gpt', 'oss', 'llm', 'api', 'vl', 'ai', 'it', 'moe', 'instruct', 'rag', 'tts', 'stt', 'ocr']);
  return words.map((word) => {
    const lower = word.toLowerCase();
    if (/^\d+(?:\.\d+)?b$/i.test(word)) return word.toUpperCase();
    if (/^v\d+$/i.test(word)) return word.toUpperCase();
    if (!acronyms.has(lower)) return lower.charAt(0).toUpperCase() + lower.slice(1);
    if (lower === 'gpt') return 'GPT';
    if (lower === 'oss') return 'OSS';
    if (lower === 'instruct') return 'Instruct';
    if (lower === 'llm') return 'LLM';
    if (lower === 'api') return 'API';
    if (lower === 'vl') return 'VL';
    if (lower === 'ai') return 'AI';
    if (lower === 'it') return 'IT';
    if (lower === 'moe') return 'MoE';
    if (lower === 'rag') return 'RAG';
    if (lower === 'tts') return 'TTS';
    if (lower === 'stt') return 'STT';
    if (lower === 'ocr') return 'OCR';
    return word;
  }).join(' ');
}

function keyOf(p, m) {
  return txt(p.id, '\u001f', m.id);
}

export async function renderModels(mount, ctx) {
  let providers = [];
  let page = 1;
  const PAGE_SIZES = [50, 100, 250, 500];

  const provSel = h('select', { class: 'input models-provider-filter', 'aria-label': 'Filter by provider' });
  const sourceSel = h('select', { class: 'input models-source-filter', 'aria-label': 'Filter by source' },
    h('option', { value: '' }, 'all sources'),
    ...SOURCES.map((source) => h('option', { value: source }, source)));
  const search = h('input', {
    class: 'input',
    type: 'search',
    placeholder: 'Search model id or label…',
    'aria-label': 'Search model id or label',
    'aria-controls': 'models-table',
    autocomplete: 'off',
    spellcheck: 'false',
  });
  const hiddenFilter = toggle('Hidden only', {
    id: 'models-hidden-only',
    help: 'Show only models currently hidden from the relay listing.',
  });
  const hiddenOnly = hiddenFilter.input;

  const count = h('span', { class: 'models-count', role: 'status', 'aria-live': 'polite' }, 'Loading…');

  const testAllBtn = h('button', { class: 'btn models-test-all', type: 'button' }, 'Test all models');
  const hideFailedToggle = toggle('Hide failed models', {
    id: 'models-hide-failed',
    help: 'Automatically hide any model that fails the test from the relay listing.',
  });
  hideFailedToggle.row.classList.add('models-hide-failed-toggle');
  testAllBtn.disabled = true;

  const resyncAll = h('button', { class: 'btn', type: 'button' }, 'Resync all models');
  const deleteAll = h('button', { class: 'btn btn-danger', type: 'button' }, 'Delete all models');
  resyncAll.disabled = true;
  deleteAll.disabled = true;
  const bulkBusy = { value: false };

  const headline = h('h1', { class: 'models-headline' }, 'Loading model catalogue…');
  const coverage = h('p', { class: 'models-coverage' }, 'Reading providers and their models.');
  const head = h('div', { class: 'models-head' },
    h('div', { class: 'models-head-copy' }, headline, coverage),
    h('div', { class: 'models-bulk' }, hideFailedToggle.row, testAllBtn, resyncAll, deleteAll));

  const filters = h('div', { class: 'models-filters' }, provSel, sourceSel, hiddenFilter.row);
  hiddenFilter.row.classList.add('models-hidden-filter');
  const controls = h('div', { class: 'models-controls' },
    h('div', { class: 'models-search' }, search),
    filters,
    h('span', { class: 'spacer' }),
    count);

  const tableWrap = h('div', { class: 'table-scroll models-table-scroll' },
    h('div', { class: 'models-loading', role: 'status' }, 'Loading model catalogue…'));
  const pager = h('div', { class: 'pager', hidden: true });
  const note = h('p', { class: 'models-note hint', role: 'note' },
    'Model tests send the fixed prompt reply with "yes" only after you click Test. Requests may consume provider quota.');
  note.hidden = true;

  const pageSizeSel = h('select', { class: 'input pager-size', 'aria-label': 'Rows per page' },
    ...PAGE_SIZES.map((size) => h('option', { value: String(size) }, String(size))));
  pageSizeSel.value = '100';

  mount.replaceChildren(head, controls, tableWrap, pager, note);

  let combos = [];
  async function refreshCombos() {
    try {
      combos = extractList(await api.get('/api/combos'), 'combos');
    } catch {
      combos = [];
    }
  }
  void refreshCombos();

  async function addModelToCombo(combo, p, m, btn = null) {
    const textEl = btn ? btn.querySelector('.models-combo-btn-text') : null;
    const originalText = textEl?.textContent;
    if (btn) {
      btn.disabled = true;
      if (textEl) textEl.textContent = 'Adding…';
    }

    try {
      const freshList = extractList(await api.get('/api/combos'), 'combos');
      const target = freshList.find((c) => c.id === combo.id) ?? combo;
      const steps = Array.isArray(target.steps) ? [...target.steps] : [];

      steps.push({
        enabled: true,
        provider: p.id,
        model: m.id,
        weight: 0,
      });

      const payload = {
        ...target,
        steps,
      };

      await api.put('/api/combos/' + encodeURIComponent(target.id), payload);
      ctx.toast.success(txt('Added ', p.id, '/', m.id, ' to combo ', target.name, '.'));
      await refreshCombos();
    } catch (err) {
      ctx.toast.error(txt('Could not add to combo ', combo.name), err);
    } finally {
      if (btn) {
        btn.disabled = false;
        if (textEl) textEl.textContent = originalText || 'Add to combo';
      }
    }
  }

  async function promptNewComboForModel(p, m) {
    const rawTail = m.id.slice(m.id.lastIndexOf('/') + 1) || m.id;
    const defaultName = rawTail.replace(/[^a-zA-Z0-9_-]/g, '-').toLowerCase();

    const name = await (ctx.prompt ? ctx.prompt({
      title: 'New combo with this model',
      message: txt('Create a new routing combo with step ', p.id, '/', m.id, ':'),
      defaultValue: defaultName,
      placeholder: 'combo-name',
      confirmLabel: 'Create combo',
      cancelLabel: 'Cancel',
    }) : null);

    if (!name || !name.trim()) return;

    try {
      const payload = {
        name: name.trim(),
        label: deriveDisplayName(name.trim()),
        strategy: 'priority',
        midstream_policy: 'restart',
        max_retries: 1,
        retry_delay_ms: 0,
        affinity: true,
        enabled: true,
        steps: [{
          enabled: true,
          provider: p.id,
          model: m.id,
          weight: 0,
        }],
        hedge: {
          enabled: false,
          participants: [],
          count: 3,
          window_ms: 800,
          winner_metric: 'first-response-byte',
          on_winner_fail: { 'pick-highest-passed': null },
          cancel_losers: true,
          loser_grace_ms: 0,
        },
      };

      const created = await api.post('/api/combos', payload);
      ctx.toast.success(txt('Created combo ', payload.name, ' with step ', p.id, '/', m.id, '.'));
      await refreshCombos();
    } catch (err) {
      ctx.toast.error('Could not create combo', err);
    }
  }

  function buildAddToComboDropdown(p, m, { className = 'btn btn-sm' } = {}) {
    const wrap = h('div', { class: 'models-combo-dropdown-wrap' });
    const btn = h('button', {
      class: className + ' models-combo-dropdown-btn',
      type: 'button',
      'aria-haspopup': 'menu',
      'aria-expanded': 'false',
      title: txt('Add ', m.id, ' to a routing combo'),
    },
      h('span', { class: 'models-combo-btn-text' }, 'Add to combo'),
      h('span', { class: 'models-combo-caret', 'aria-hidden': 'true' }, ' ▾'));

    const menu = h('div', {
      class: 'models-combo-menu',
      role: 'menu',
      'aria-label': 'Select a combo to add this model to',
      hidden: true,
    });

    let open = false;

    function closeMenu() {
      if (!open) return;
      open = false;
      menu.hidden = true;
      btn.setAttribute('aria-expanded', 'false');
      wrap.classList.remove('is-open');
      document.removeEventListener('click', onDocClick);
      document.removeEventListener('keydown', onDocKey);
    }

    function onDocClick(e) {
      if (!wrap.contains(e.target)) closeMenu();
    }

    function onDocKey(e) {
      if (e.key === 'Escape') {
        e.stopPropagation();
        closeMenu();
        btn.focus({ preventScroll: true });
      }
    }

    async function toggleMenu(e) {
      e?.stopPropagation();
      if (open) {
        closeMenu();
        return;
      }

      document.querySelectorAll('.models-combo-dropdown-wrap.is-open').forEach((el) => {
        el.querySelector('.models-combo-menu')?.setAttribute('hidden', '');
        el.querySelector('.models-combo-dropdown-btn')?.setAttribute('aria-expanded', 'false');
        el.classList.remove('is-open');
      });

      if (!combos.length) await refreshCombos();

      renderMenuItems();
      open = true;
      menu.hidden = false;
      btn.setAttribute('aria-expanded', 'true');
      wrap.classList.add('is-open');

      const rect = btn.getBoundingClientRect();
      if (window.innerWidth - rect.left < 280) {
        menu.classList.add('is-right');
      } else {
        menu.classList.remove('is-right');
      }

      setTimeout(() => {
        document.addEventListener('click', onDocClick);
        document.addEventListener('keydown', onDocKey);
        const first = menu.querySelector('button');
        if (first) first.focus({ preventScroll: true });
      }, 0);
    }

    function renderMenuItems() {
      menu.replaceChildren();
      const head = h('div', { class: 'models-combo-menu-head' }, 'ADD TO COMBO');
      menu.append(head);

      if (!combos.length) {
        menu.append(h('div', { class: 'models-combo-menu-empty hint' }, 'No combos configured yet'));
      } else {
        for (const c of combos) {
          const stepCount = c.steps?.length ?? 0;
          const isInCombo = c.steps?.some((s) => s.provider === p.id && s.model === m.id);
          const item = h('button', {
            class: 'models-combo-menu-item' + (isInCombo ? ' is-in-combo' : ''),
            type: 'button',
            role: 'menuitem',
          },
            h('span', { class: 'models-combo-item-name mono' }, c.name),
            h('span', { class: 'models-combo-item-meta' },
              txt(stepCount, ' step', stepCount === 1 ? '' : 's'),
              isInCombo ? chip('added', 'chip-neutral models-combo-tag') : null));

          item.addEventListener('click', async (e) => {
            e.stopPropagation();
            closeMenu();
            await addModelToCombo(c, p, m, btn);
          });
          menu.append(item);
        }
      }

      const div = h('div', { class: 'models-combo-menu-divider' });
      const newItem = h('button', {
        class: 'models-combo-menu-item models-combo-menu-new',
        type: 'button',
        role: 'menuitem',
      }, '+ New combo with this model…');

      newItem.addEventListener('click', async (e) => {
        e.stopPropagation();
        closeMenu();
        await promptNewComboForModel(p, m);
      });

      menu.append(div, newItem);
    }

    btn.addEventListener('click', toggleMenu);
    wrap.append(btn, menu);
    return wrap;
  }

  let selectedKey = null;
  let detailRow = null;

  function closeDetail({ restoreFocus = true } = {}) {
    const previous = selectedKey;
    selectedKey = null;
    if (detailRow) {
      detailRow.remove();
      detailRow = null;
    }
    paintSelection();
    if (restoreFocus) focusRow(previous);
  }

  function openDetail(p, m) {
    const key = keyOf(p, m);
    if (detailRow && selectedKey === key) {
      closeDetail();
      return;
    }
    closeDetail({ restoreFocus: false });
    selectedKey = key;
    paintSelection();

    const anchorRow = tableWrap.querySelector(`tbody tr[data-key="${CSS.escape(key)}"]`);
    if (!anchorRow) return;

    const panel = buildInlineModelDetail(p, m);
    const colCount = anchorRow.children.length || 8;
    detailRow = h('tr', { class: 'model-detail-row' },
      h('td', { colspan: String(colCount) }, panel));

    anchorRow.after(detailRow);
    requestAnimationFrame(() => {
      if (detailRow?.isConnected) {
        detailRow.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      }
    });
  }

  function fact(label, value, { title = null, missing = false } = {}) {
    return h('div', { class: txt('model-fact', missing ? ' is-missing' : '') },
      h('dt', {}, label),
      h('dd', { title: title ?? null }, value));
  }

  function buildInlineModelDetail(p, m) {
    const label = labelText(m);
    const derived = deriveDisplayName(m.id);
    const heading = label || derived || 'Model detail';

    const panel = h('div', {
      class: 'model-inline-detail',
      role: 'region',
      'aria-label': txt('Detail for ', m.id),
      tabindex: '-1',
    });

    const closeBtn = h('button', {
      class: 'icon-btn',
      type: 'button',
      'aria-label': 'Close model detail',
      title: 'Close (Escape)',
    }, icon('x'));
    closeBtn.addEventListener('click', () => closeDetail());

    const titleBlock = h('div', { class: 'model-inline-title' },
      h('h2', { title: heading }, heading),
      h('span', { class: 'model-inline-id mono', title: m.id }, m.id),
      copyButton(m.id, { label: 'Copy id', className: 'btn btn-sm' }));

    const head = h('div', { class: 'model-inline-head' },
      titleBlock,
      h('span', { class: 'spacer' }),
      closeBtn);

    const source = m.source ?? 'synced';
    const synced = m.synced_at ? formatAge(m.synced_at) : '';
    const ctxLen = contextLength(m);
    const out = maxOutput(m);
    const { input: priceIn, output: priceOut } = priceParts(m);
    const levels = modelLevels(m, p);
    const declared = declaredLevels(m);
    const effectiveDefault = REASONING_LEVELS.includes(m.effective_default_reasoning_level)
      ? m.effective_default_reasoning_level
      : '';
    const inputs = modalities(m);

    const identity = h('section', { class: 'model-inline-section' },
      h('h3', {}, 'Identity'),
      h('dl', { class: 'model-facts' },
        fact('Model id', h('code', { class: 'mono' }, m.id)),
        fact('Provider', h('span', { class: 'models-inline' },
          h('code', { class: 'mono' }, p.id),
          p.label ? h('span', { class: 'models-muted' }, txt(' · ', p.label)) : null)),
        fact('Source', source),
        fact('Visibility', m.hidden ? 'Hidden: excluded from relay listing' : 'Listed in relay'),
        fact('Last synced', synced || 'Not reported', { missing: !synced })));

    const pricingFacts = priceIn === null && priceOut === null
      ? [fact('Pricing', 'Not reported by provider', { missing: true })]
      : [
        fact('Input price', priceIn === null ? 'Not reported' : '$' + fmt(priceIn) + ' / 1M tokens', { missing: priceIn === null }),
        fact('Output price', priceOut === null ? 'Not reported' : '$' + fmt(priceOut) + ' / 1M tokens', { missing: priceOut === null }),
      ];

    const capabilities = h('section', { class: 'model-inline-section' },
      h('h3', {}, 'Reported capabilities'),
      h('dl', { class: 'model-facts' },
        fact('Context length', ctxLen === null ? 'Not reported' : fmt(ctxLen) + ' tokens', { missing: ctxLen === null }),
        ...pricingFacts,
        fact('Max output', out === null ? 'Not reported' : fmt(out) + ' tokens', { missing: out === null })));

    const levelChips = levels.length
      ? h('span', { class: 'models-chip-row' }, ...levels.map((level) => chip(
        level,
        level === effectiveDefault ? 'chip-pink' : 'chip-neutral',
        level === effectiveDefault ? 'Default reasoning level' : 'Reasoning level',
      )))
      : 'No reasoning levels advertised.';

    const declaredText = declared.length ? declared.join(', ') : 'Derived from provider';
    const inputsText = inputs.length
      ? h('span', { class: 'models-chip-row' }, ...inputs.map((modality) => chip(modality, 'chip-neutral')))
      : 'Derives from server';

    const reasoning = h('section', { class: 'model-inline-section' },
      h('h3', {}, 'Reasoning & input'),
      h('dl', { class: 'model-facts' },
        fact('Effective levels', levelChips),
        fact('Default', effectiveDefault || 'Inherits global default'),
        fact('Declared', declaredText),
        fact('Input modalities', inputsText)));

    const metadata = h('section', { class: 'model-inline-section model-inline-meta-section' },
      h('h3', {}, 'Metadata overrides'),
      modelMetadataEditor(p, m));

    // Actions
    const addToCombo = buildAddToComboDropdown(p, m, { className: 'btn btn-primary btn-sm' });
    const test = h('button', {
      class: 'btn btn-sm',
      type: 'button',
      'aria-label': txt('Test ', m.id, ' on provider ', p.id),
    }, 'Test model');
    test.addEventListener('click', () => testModel(p, m, test));

    const del = h('button', {
      class: 'btn btn-danger btn-sm',
      type: 'button',
      'aria-label': txt('Delete ', m.id, ' from provider ', p.id),
    }, 'Delete model');
    del.addEventListener('click', () => deleteModel(p, m));

    const hide = toggle('Hidden from relay listing', {
      checked: Boolean(m.hidden),
      help: 'Hidden models remain configured but are excluded from the relay model listing.',
    });
    hide.row.classList.add('models-detail-hide');
    hide.input.addEventListener('change', () => toggleHidden(p, m, hide.input));

    const actionsBar = h('div', { class: 'model-inline-actions-bar' },
      h('div', { class: 'model-inline-actions-left' }, addToCombo, test, del, hide.row),
      h('span', { class: 'hint' }, 'Testing sends a small prompt and may consume provider quota.'));

    const grid = h('div', { class: 'model-inline-grid' },
      identity,
      capabilities,
      reasoning,
      metadata);

    const body = h('div', { class: 'model-inline-body' }, actionsBar, grid);

    panel.append(head, body);

    panel.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        closeDetail();
      }
    });

    return panel;
  }

  function paintSelection() {
    for (const row of tableWrap.querySelectorAll('tbody tr[data-key]')) {
      const active = row.dataset.key === selectedKey;
      row.classList.toggle('row-selected', active);
      row.setAttribute('aria-expanded', String(active));
      if (active) row.setAttribute('aria-current', 'true');
      else row.removeAttribute('aria-current');
    }
  }

  function focusRow(key) {
    if (key) {
      const row = Array.from(tableWrap.querySelectorAll('tbody tr[data-key]'))
        .find((candidate) => candidate.dataset.key === key);
      if (row) {
        row.focus({ preventScroll: true });
        row.scrollIntoView({ block: 'nearest' });
        return;
      }
    }
    search.focus({ preventScroll: true });
  }

  function findModel(key) {
    for (const p of providers) {
      for (const m of (p.models ?? [])) {
        if (keyOf(p, m) === key) return { p, m };
      }
    }
    return null;
  }

  function refreshDrawer() {
    if (!selectedKey) return;
    const found = findModel(selectedKey);
    if (!found) {
      closeDetail({ restoreFocus: false });
      return;
    }
    renderDetail(found.p, found.m);
  }

  // ------------------------------------------------------------------ filters and rows

  function allRows() {
    return providers.flatMap((p) => (p.models ?? []).map((m) => ({ p, m })));
  }

  function rows() {
    const providerId = provSel.value;
    const source = sourceSel.value;
    const query = search.value.trim().toLowerCase();
    let out = allRows();
    if (providerId) out = out.filter(({ p }) => p.id === providerId);
    if (source) out = out.filter(({ m }) => (m.source ?? 'synced') === source);
    if (hiddenOnly.checked) out = out.filter(({ m }) => Boolean(m.hidden));
    if (query) {
      out = out.filter(({ m }) => (m.id + ' ' + (m.label ?? '')).toLowerCase().includes(query));
      return out.sort(compareModelRows);
    }
    // Alphabetical order made the first page a single alphabetical slice of the catalogue, where
    // two sparse columns could be entirely blank. Put rows that fill those columns first; id order
    // still breaks every tie, and a search result stays in plain id order so a known model is findable.
    const ranked = out.map((item) => ({
      item,
      fills: (contextLength(item.m) !== null ? 2 : 0) + (modelLevels(item.m, item.p).length ? 1 : 0),
    }));
    ranked.sort((a, b) => b.fills - a.fills || compareModelRows(a.item, b.item));
    return ranked.map((entry) => entry.item);
  }

  function updateSummary() {
    const total = providers.reduce((sum, p) => sum + (p.models?.length ?? 0), 0);
    const labelCount = providers.reduce((sum, p) => sum
      + (p.models ?? []).filter((m) => labelText(m)).length, 0);
    const ctxCount = providers.reduce((sum, p) => sum
      + (p.models ?? []).filter((m) => contextLength(m) !== null).length, 0);
    const priceCount = providers.reduce((sum, p) => sum
      + (p.models ?? []).filter((m) => hasPrice(m)).length, 0);
    const hiddenCount = providers.reduce((sum, p) => sum
      + (p.models ?? []).filter((m) => Boolean(m.hidden)).length, 0);

    resyncAll.disabled = bulkBusy.value || providers.length === 0;
    deleteAll.disabled = bulkBusy.value || total === 0;
    testAllBtn.disabled = bulkBusy.value || total === 0;

    if (!providers.length) {
      headline.textContent = 'No providers configured';
      coverage.textContent = 'Add a provider, then sync its catalogue to populate this page.';
      search.placeholder = 'Search models by id or label…';
      return;
    }
    if (!total) {
      headline.textContent = 'No models synced yet';
      coverage.textContent = txt(providers.length, providers.length === 1 ? ' provider' : ' providers',
        ' configured, but none reports a model yet.');
      search.placeholder = 'Search models by id or label…';
      return;
    }

    headline.replaceChildren(
      h('strong', { class: 'models-headline-number' }, fmt(total)),
      ' models across ',
      h('strong', { class: 'models-headline-number' }, fmt(providers.length)),
      providers.length === 1 ? ' provider' : ' providers',
    );
    // Stats, not prose: three facts the provider payload can actually answer, in one scan line.
    const coverageParts = [
      txt('labels ', fmt(labelCount)),
      txt('context ', fmt(ctxCount)),
      priceCount ? txt('pricing ', fmt(priceCount)) : 'pricing not reported',
    ];
    if (hiddenCount) coverageParts.push(txt(fmt(hiddenCount), ' hidden'));
    coverage.textContent = txt('Provider coverage · ', coverageParts.join(' · '));
    coverage.title = 'Counts across all loaded models. Rows that report context or reasoning are listed first.';
    search.placeholder = txt('Search ', fmt(total), ' models by id or label…');
  }

  function updateCount(total, shown) {
    const filtering = Boolean(search.value.trim() || provSel.value || sourceSel.value || hiddenOnly.checked);
    if (!total || (shown === total && !filtering)) {
      // The headline already carries the unfiltered total; a second copy in the toolbar is noise.
      count.textContent = '';
      count.title = '';
    } else {
      count.textContent = txt(fmt(shown), ' of ', fmt(total), ' models');
      count.title = 'Models matching the current filters.';
    }
  }

  function clearFilters() {
    search.value = '';
    provSel.value = '';
    sourceSel.value = '';
    hiddenOnly.checked = false;
    page = 1;
    renderTable(null, true);
    search.focus({ preventScroll: true });
  }

  function emptyActions(primary, secondary = null) {
    return h('div', { class: 'models-empty-actions' }, primary, secondary);
  }

  function renderEmpty(total) {
    pager.hidden = true;
    note.hidden = true;
    if (!total) {
      if (!providers.length) {
        const openProviders = h('button', { class: 'btn btn-primary btn-sm', type: 'button' }, 'Open providers');
        openProviders.addEventListener('click', () => ctx.navigate('#/providers'));
        tableWrap.replaceChildren(emptyState(
          'No providers configured yet.',
          'Models belong to providers. Add one, then sync its catalogue and this page will fill in.',
        ));
        tableWrap.querySelector('.empty-state')?.append(emptyActions(openProviders));
        return;
      }
      const sync = h('button', { class: 'btn btn-primary btn-sm', type: 'button' }, 'Sync all providers');
      sync.addEventListener('click', () => resyncAllModels());
      const openProviders = h('button', { class: 'btn btn-sm', type: 'button' }, 'Open providers');
      openProviders.addEventListener('click', () => ctx.navigate('#/providers'));
      tableWrap.replaceChildren(emptyState(
        'No models synced yet.',
        txt(providers.length, providers.length === 1 ? ' is' : 's are',
          ' configured, but none reports a model. Syncing asks each provider for its current catalogue.'),
      ));
      tableWrap.querySelector('.empty-state')?.append(emptyActions(sync, openProviders));
      return;
    }

    const active = [];
    const query = search.value.trim();
    if (query) active.push('search “' + query + '”');
    if (provSel.value) active.push('provider ' + provSel.value);
    if (sourceSel.value) active.push('source ' + sourceSel.value);
    if (hiddenOnly.checked) active.push('hidden only');
    const detail = active.length
      ? 'Nothing matches ' + active.join(' + ') + '.'
      : 'Nothing matches the current filters.';
    const clear = h('button', { class: 'btn btn-sm', type: 'button' }, 'Clear filters');
    clear.addEventListener('click', clearFilters);
    tableWrap.replaceChildren(emptyState(
      'No models match these filters.',
      txt(detail, ' Clear the filters to see all ', fmt(total), ' models.'),
    ));
    tableWrap.querySelector('.empty-state')?.append(emptyActions(clear));
  }

  function renderTable(focus = null, resetScroll = false) {
    const total = providers.reduce((sum, p) => sum + (p.models?.length ?? 0), 0);
    let list = [];
    try {
      list = rows();
    } catch {
      list = [];
    }
    updateCount(total, list.length);

    if (selectedKey && !list.some(({ p, m }) => keyOf(p, m) === selectedKey)) {
      closeDetail({ restoreFocus: false });
    }

    if (!list.length) {
      renderEmpty(total);
      return;
    }

    let labelCount = 0;
    let ctxCount = 0;
    let priceCount = 0;
    let levelCount = 0;
    let inputCount = 0;
    for (const { p, m } of list) {
      if (labelText(m)) labelCount += 1;
      if (contextLength(m) !== null) ctxCount += 1;
      if (hasPrice(m)) priceCount += 1;
      if (modelLevels(m, p).length) levelCount += 1;
      if (modalities(m).length) inputCount += 1;
    }
    const showLabel = labelCount > 0;
    const showCtx = ctxCount > 0;
    const showPrice = priceCount > 0;
    const showLevels = levelCount > 0;
    const showInputs = inputCount > 0;
    const showReasoning = showLevels || showInputs;
    const share = (n) => Math.round((n / list.length) * 100) + '%';
    const bounds = pageBounds(list.length, page, Number(pageSizeSel.value));
    page = bounds.page;
    const visible = list.slice(bounds.start, bounds.end);

    const colgroup = h('colgroup', {},
      h('col', { class: 'models-col-model' }),
      showLabel ? h('col', { class: 'models-col-label' }) : null,
      h('col', { class: 'models-col-provider' }),
      h('col', { class: 'models-col-source' }),
      showCtx ? h('col', { class: 'models-col-ctx' }) : null,
      showPrice ? h('col', { class: 'models-col-price' }) : null,
      showReasoning ? h('col', { class: 'models-col-reasoning' }) : null,
      h('col', { class: 'models-col-actions' }));

    const header = h('tr', {},
      h('th', { scope: 'col' }, 'Model'),
      showLabel ? h('th', {
        scope: 'col',
        title: txt(fmt(labelCount), ' of ', fmt(list.length), ' models report a provider label; the rest show a name derived from the model id.'),
      }, 'Label') : null,
      h('th', { scope: 'col' }, 'Provider'),
      h('th', { scope: 'col' }, 'Source'),
      showCtx ? h('th', {
        scope: 'col',
        class: 'num',
        title: txt(share(ctxCount), ' of models (', fmt(ctxCount), ' of ', fmt(list.length), ') report a context window. Blank cells mean this provider does not report one.'),
      }, 'Ctx') : null,
      showPrice ? h('th', { scope: 'col', class: 'num', title: 'Price per million input and output tokens, where reported' }, '$/1M in · out') : null,
      showReasoning ? h('th', {
        scope: 'col',
        title: showLevels && showInputs
          ? txt(share(levelCount), ' of models (', fmt(levelCount), ' of ', fmt(list.length), ') advertise reasoning levels; the column also shows accepted input types. Blank cells mean neither is advertised.')
          : showLevels
            ? txt(share(levelCount), ' of models (', fmt(levelCount), ' of ', fmt(list.length), ') advertise reasoning levels. Blank cells mean none are advertised.')
            : 'Input types the model accepts',
      }, showLevels ? 'Reasoning' : 'Input') : null,
      h('th', { scope: 'col', class: 'models-actions-head' }, 'Actions'));

    const tbody = h('tbody');
    for (const { p, m } of visible) {
      const key = keyOf(p, m);
      const label = labelText(m);
      const derived = label ? '' : deriveDisplayName(m.id);
      const ctx = contextLength(m);
      const price = priceText(m);
      const levels = modelLevels(m, p).slice().reverse();
      const shownLevels = levels.slice(0, 3);
      const remainingLevels = levels.length - shownLevels.length;
      const inputs = modalities(m);

      const labelCell = h('td', {
        class: 'models-label-cell',
        title: label || (derived
          ? 'No provider label; this name is derived from the model id.'
          : 'This provider reports no label for this model.'),
      }, label
        ? label
        : derived
          ? h('span', { class: 'models-derived-name' }, derived)
          : h('span', { class: 'models-muted' }, 'No label'));

      const rowAddToCombo = buildAddToComboDropdown(p, m, { className: 'btn btn-sm' });

      const test = h('button', {
        class: 'btn btn-primary btn-sm',
        type: 'button',
        'aria-label': txt('Test ', m.id, ' on provider ', p.id),
      }, 'Test');
      test.addEventListener('click', () => testModel(p, m, test));

      // A question mark on every row reads as an unexplained control; the label is the control,
      // and the title carries the consequence for the few who need it.
      const visibility = toggle('Hidden', { checked: Boolean(m.hidden) });
      visibility.row.classList.add('models-visibility');
      visibility.row.title = 'Hide this model from the relay model listing without deleting it.';
      visibility.input.title = visibility.row.title;
      visibility.input.addEventListener('change', () => toggleHidden(p, m, visibility.input));

      const del = h('button', {
        class: 'btn btn-danger btn-sm',
        type: 'button',
        'aria-label': txt('Delete ', m.id, ' from provider ', p.id),
      }, 'Delete');
      del.addEventListener('click', () => deleteModel(p, m));

      const row = h('tr', {
        class: 'models-row',
        tabindex: '0',
        'aria-expanded': String(selectedKey === key),
        title: txt('Open details for ', m.id),
        dataset: { key },
      },
      h('td', { class: 'models-model-cell' },
        h('div', { class: 'models-model-line' },
          h('span', { class: 'mono models-id', title: m.id }, m.id),
          h('span', { class: 'models-chevron', 'aria-hidden': 'true' }, '›'))),
      showLabel ? labelCell : null,
      h('td', { class: 'models-provider-cell' },
        chip(p.id, 'chip-neutral models-provider', p.label ? 'Provider: ' + p.label : 'Provider ' + p.id)),
      h('td', { class: 'models-source-cell' },
        h('span', { class: 'models-source', title: 'Provenance: synced, entered manually, or inferred by the server.' }, m.source ?? 'synced')),
      // Absence is quiet in a dense table: the column only exists because some rows report it,
      // so a blank cell reads as "not reported" without turning the column into em dashes.
      showCtx ? h('td', {
        class: 'num models-ctx',
        title: ctx === null ? 'Context length is not reported for this model.' : null,
      }, ctx === null
        ? h('span', { class: 'sr-only' }, 'Not reported')
        : h('span', { title: fmt(ctx) + ' tokens' }, formatCompact(ctx))) : null,
      showPrice ? h('td', {
        class: 'num models-price',
        title: price ? priceTitle(m) : 'Pricing is not reported for this model.',
      }, price || h('span', { class: 'sr-only' }, 'Not reported')) : null,
      showReasoning ? h('td', { class: 'models-reasoning-cell' },
        showLevels && levels.length
          ? h('span', {
            class: 'models-levels',
            title: 'All levels: ' + levels.join(', '),
          }, txt(shownLevels.join(' · '), remainingLevels ? ' +' + remainingLevels : ''))
          : null,
        showInputs && inputs.length
          ? h('span', { class: 'models-modalities', title: 'Accepted input: ' + inputs.join(', ') },
            'input: ' + inputs.join(', '))
          : !levels.length && !inputs.length ? h('span', { class: 'sr-only' }, 'Not reported') : null) : null,
      h('td', { class: 'actions models-actions-cell' },
        h('div', { class: 'models-actions-inner' }, rowAddToCombo, test, visibility.row, del)));

      row.classList.toggle('is-hidden', Boolean(m.hidden));
      row.addEventListener('click', (event) => {
        if (event.target.closest?.('button, a, input, select, label, summary, details, .model-metadata')) return;
        openDetail(p, m);
      });
      row.addEventListener('keydown', (event) => {
        if (event.target !== row) return;
        if (event.key !== 'Enter' && event.key !== ' ') return;
        event.preventDefault();
        openDetail(p, m);
      });
      tbody.append(row);
    }

    const table = h('table', {
      class: 'tbl models-table',
      id: 'models-table',
    },
    h('caption', { class: 'sr-only' }, txt('Models: ', fmt(list.length), ' matching rows')),
    colgroup,
    h('thead', {}, header),
    tbody);

    if (selectedKey) {
      const match = visible.find(({ p, m }) => keyOf(p, m) === selectedKey);
      if (match) {
        const anchor = tbody.querySelector(`tr[data-key="${CSS.escape(selectedKey)}"]`);
        if (anchor) {
          const panel = buildInlineModelDetail(match.p, match.m);
          detailRow = h('tr', { class: 'model-detail-row' },
            h('td', { colspan: String(colgroup.children.length || 8) }, panel));
          anchor.after(detailRow);
        }
      } else {
        detailRow = null;
      }
    }
    tableWrap.replaceChildren(table);
    paintSelection();
    if (resetScroll) tableWrap.scrollTop = 0;
    renderPager(list.length, bounds, focus);
    note.hidden = false;
  }

  function renderPager(total, bounds, focus) {
    pager.hidden = false;
    const go = (next, nextFocus) => { page = next; renderTable(nextFocus, true); };

    const prev = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': 'Previous page' }, 'Previous');
    prev.disabled = page <= 1;
    prev.addEventListener('click', () => go(page - 1, 'prev'));

    const next = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': 'Next page' }, 'Next');
    next.disabled = page >= bounds.pages;
    next.addEventListener('click', () => go(page + 1, 'next'));

    const pageSel = h('select', { class: 'input pager-page', 'aria-label': 'Page' },
      ...Array.from({ length: bounds.pages }, (_, i) => h('option', { value: String(i + 1) }, String(i + 1))));
    pageSel.value = String(page);
    pageSel.addEventListener('change', () => go(Number(pageSel.value), 'page'));

    pager.replaceChildren(
      h('span', { class: 'pager-range' }, txt(fmt(bounds.start + 1), '–', fmt(bounds.end), ' of ', fmt(total))),
      h('span', { class: 'spacer' }),
      h('label', { class: 'pager-control' }, 'Rows ', pageSizeSel),
      prev,
      h('label', { class: 'pager-control' }, 'Page ', pageSel),
      h('span', { class: 'pager-pages' }, txt('of ', fmt(bounds.pages))),
      next,
    );

    // Rebuilding the bar detaches the control that was just used. Put focus back on it so a
    // keyboard page turn does not fall back to the document body.
    if (focus === 'prev') prev.focus();
    else if (focus === 'next') next.focus();
    else if (focus === 'page') pageSel.focus();
    else if (focus === 'size') pageSizeSel.focus();
  }

  // ------------------------------------------------------------------ metadata

  function modelMetadataEditor(p, m) {
    const dialectLevels = providerLevels(p, m);
    const configuredLevels = declaredLevels(m);
    const effectiveLevels = modelLevels(m, p);
    const levelsDerived = configuredLevels.length === 0;
    const modalitiesDerived = !Array.isArray(m.input_modalities) || m.input_modalities.length === 0;
    const configuredModalities = Array.isArray(m.input_modalities)
      ? m.input_modalities.filter((modality) => INPUT_MODALITIES.includes(modality))
      : [];
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
      ? txt(' Declared but unsendable on this provider: ', unsendableLevels.join(', '), '; saving will drop it.')
      : '';
    // Validation refuses a default the declared list does not contain, so say so before the save
    // turns into a 400 with the same words.
    const strayDefaultNote = defaultLevel && configuredLevels.length && !configuredLevels.includes(defaultLevel)
      ? txt(' The default ', defaultLevel, ' is not one of the declared levels; check it or clear it to inherit.')
      : '';
    const levels = new Set(configuredLevels);
    const selectedModalities = new Set(configuredModalities);
    const levelInputs = [];
    const modalityInputs = [];
    const defaultSelect = h('select', { class: 'input', 'aria-label': txt('Default reasoning level for ', m.id) },
      h('option', { value: '' }, 'inherit global default'),
      ...defaultChoices.map((level) => h('option', { value: level }, level)));
    defaultSelect.value = defaultLevel;
    const save = h('button', { class: 'btn btn-sm', type: 'button' }, 'Save metadata');

    const editor = h('div', { class: 'model-metadata model-metadata-open' },
      h('p', { class: 'hint' },
        txt('This ', p.dialect, ' provider can send: ', dialectLevels.join(', '),
          '. The model currently offers: ', effectiveLevels.length ? effectiveLevels.join(', ') : 'no reasoning levels',
          levelsDerived ? ' (derived; nothing is declared on this entry)' : '',
          '.', unsendableNote, strayDefaultNote, ' Runs at ', effectiveDefault || defaultLevelLabel,
          ' when the name carries no level. Input modalities: ',
          modalitiesDerived ? 'automatic (derived by server)' : configuredModalities.join(', '),
          '. Saving writes the checked list explicitly.')),
      h('span', { class: 'field-label' }, 'Supported reasoning levels'),
      h('div', { class: 'model-meta-options' }, ...dialectLevels.map((level) => {
        const control = toggle(level, { checked: levels.has(level) });
        control.input.value = level;
        control.input.addEventListener('change', () => (control.input.checked ? levels.add(level) : levels.delete(level)));
        levelInputs.push(control.input);
        return control.row;
      })),
      field('Per-model default reasoning level', defaultSelect, {
        hint: 'Choose a level this provider can send, or inherit the global default.',
      }),
      h('span', { class: 'field-label' }, 'Accepted input modalities'),
      h('div', { class: 'model-meta-options' }, ...INPUT_MODALITIES.map((modality) => {
        const control = toggle(modality, { checked: selectedModalities.has(modality) });
        control.input.value = modality;
        control.input.addEventListener('change', () => (control.input.checked
          ? selectedModalities.add(modality)
          : selectedModalities.delete(modality)));
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
        await api.post('/api/providers/' + encodeURIComponent(p.id) + '/models', {
          ...m,
          reasoning_levels: nextLevels,
          default_reasoning_level: defaultSelect.value || null,
          input_modalities: nextModalities,
        });
        ctx.toast.success('Metadata saved for ' + m.id + '.');
        await reload();
      } catch (err) {
        save.disabled = false;
        ctx.toast.error('Could not save model metadata', err);
      }
    });
    return editor;
  }

  // ------------------------------------------------------------------ model actions

  async function testModel(p, m, button) {
    button.disabled = true;
    const originalText = button.textContent;
    button.textContent = 'Testing…';
    try {
      const started = performance.now();
      // A provider that refuses the test with its own 401 is not a dead panel session — signing in
      // again cannot fix a rejected credential, and the bounce would hide the models list along
      // with the verdict.
      const result = await api.post('/api/models/test', {
        provider: p.id,
        model: m.id,
        prompt: MODEL_TEST_PROMPT,
      }, { skipAuthRedirect: true });
      const elapsed = Math.round(performance.now() - started);
      const reply = result?.response ?? result?.reply ?? result?.output ?? result?.message;
      if (reply === undefined || reply === null) {
        ctx.toast.success(txt('Test completed in ', elapsed, ' ms; API returned no reply text. ', JSON.stringify(result)));
      } else {
        ctx.toast.success(txt('Model replied in ', elapsed, ' ms: ', typeof reply === 'string' ? reply : JSON.stringify(reply)));
      }
    } catch (err) {
      const status = Number.isFinite(err?.status) ? 'HTTP ' + err.status : 'API error';
      const code = err?.code ? ' (' + err.code + ')' : '';
      const details = err?.details?.length
        ? ' — ' + err.details.map((item) => typeof item === 'string' ? item : JSON.stringify(item)).join('; ')
        : '';
      ctx.toast.error(txt('Model test failed: ', status, code, ': ', err?.message ?? 'request failed', details));
    } finally {
      button.disabled = false;
      button.textContent = originalText;
    }
  }

  async function toggleHidden(p, m, btn) {
    const inDrawer = Boolean(drawer && drawer.contains(btn));
    const key = keyOf(p, m);
    btn.disabled = true;
    try {
      // POST /api/providers/{id}/models is the upsert path; hidden is authored metadata and
      // manual entries win over a later sync (docs/03-configuration.md). Assumption documented
      // in panel/README.md — if the backend adds a dedicated toggle route, change it here.
      await api.post('/api/providers/' + encodeURIComponent(p.id) + '/models', { ...m, hidden: !m.hidden });
      await reload();
      if (!inDrawer) focusRow(key);
    } catch (err) {
      btn.checked = Boolean(m.hidden); // reverted: the POST failed, so the server still has the old value
      btn.disabled = false;
      ctx.toast.error('Could not change visibility', err);
    }
  }

  async function deleteModel(p, m) {
    const ok = await ctx.confirm({
      title: 'Delete model “' + m.id + '”?',
      message: m.source === 'manual'
        ? 'It was entered manually and will not come back.'
        : 'A later sync may re-add it.',
      confirmLabel: 'Delete model',
    });
    if (!ok) return;
    const wasSelected = selectedKey === keyOf(p, m);
    try {
      await api.del('/api/providers/' + encodeURIComponent(p.id) + '/models/' + encodeURIComponent(m.id));
      ctx.toast.success('Model ' + m.id + ' deleted.');
      await reload();
      if (wasSelected) search.focus({ preventScroll: true });
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }

  // ------------------------------------------------------------------ bulk actions

  function beginBulk(button, label) {
    bulkBusy.value = true;
    resyncAll.disabled = true;
    deleteAll.disabled = true;
    button.textContent = label;
  }

  function endBulk(button, label) {
    bulkBusy.value = false;
    resyncAll.disabled = false;
    deleteAll.disabled = false;
    button.textContent = label;
  }

  async function resyncAllModels() {
    if (bulkBusy.value) return;
    // Snapshot the roster: reload() replaces the providers list after the loop, and the failure
    // summary must describe the set that was actually attempted, not whatever came back.
    const targets = providers;
    const total = targets.length;
    if (!total) { ctx.toast.warning('There are no providers to resync.'); return; }
    beginBulk(resyncAll, txt('Resyncing 0/', total, '…'));
    const failed = [];
    let done = 0;
    // Sequential on purpose: model sync is an upstream call per provider, and firing 38 at once
    // is a good way to meet a provider's rate limit and get a partial refresh.
    for (const p of targets) {
      resyncAll.textContent = txt('Resyncing ', done + 1, '/', total, '…');
      try {
        await api.post('/api/providers/' + encodeURIComponent(p.id) + '/models/sync');
      } catch (err) {
        failed.push({ provider: p.id, message: err?.message ?? String(err) });
      }
      done += 1;
    }
    endBulk(resyncAll, 'Resync all models');
    try { await reload(); } catch { /* reload() already paints its own error */ }
    if (failed.length) {
      const names = failed.map((f) => f.provider).join(', ');
      ctx.toast.error(txt(failed.length, ' of ', total, ' provider syncs failed: ', names, '. ', failed[0].message));
    } else {
      ctx.toast.success(txt('Resynced models for ', total, ' provider', total === 1 ? '' : 's', '.'));
    }
  }

  async function deleteAllModels() {
    if (bulkBusy.value) return;
    // Snapshot before the confirmation and loop: reloading mid-loop would shrink the list under
    // the progress count, and the operator confirmed this exact set.
    const work = allRows();
    const total = work.length;
    if (!total) { ctx.toast.warning('There are no models to delete.'); return; }
    const ok = await ctx.confirm({
      title: txt('Delete all ', fmt(total), ' models?'),
      message: 'Every model entry is removed from every provider. Providers, accounts, client keys and combos are left alone.',
      warnLines: [
        'Combos, aliases and clients that name these models stop resolving until the models are added again or a sync brings them back.',
        'Manual model entries cannot be recovered from the panel.',
      ],
      confirmLabel: 'Delete all models',
      danger: true,
    });
    if (!ok) return;

    beginBulk(deleteAll, txt('Deleting 0/', total, '…'));
    const failed = [];
    let done = 0;
    for (const { p, m } of work) {
      // Number the request about to run, so the last one reads N/N instead of stopping at N-1/N.
      if (done === 0 || (done + 1) % 5 === 0 || done === total - 1) {
        deleteAll.textContent = txt('Deleting ', done + 1, '/', total, '…');
      }
      try {
        await api.del('/api/providers/' + encodeURIComponent(p.id) + '/models/' + encodeURIComponent(m.id));
      } catch (err) {
        failed.push({ provider: p.id, model: m.id, message: err?.message ?? String(err) });
      }
      done += 1;
    }
    endBulk(deleteAll, 'Delete all models');
    try { await reload(); } catch { /* reload() already paints its own error */ }
    if (failed.length) {
      ctx.toast.error(txt(failed.length, ' of ', total, ' model deletes failed. First: ',
        failed[0].provider, '/', failed[0].model, ' — ', failed[0].message));
    } else {
      ctx.toast.success(txt('Deleted ', total, ' model', total === 1 ? '' : 's', '.'));
    }
  }

  async function reload() {
    try {
      providers = await ctx.loadProviders(true);
    } catch (err) {
      providers = [];
      pager.hidden = true;
      note.hidden = true;
      closeDetail({ restoreFocus: false });
      headline.textContent = 'Models are unavailable';
      coverage.textContent = 'The provider catalogue could not be loaded.';
      count.textContent = 'Unavailable';
      search.placeholder = 'Search unavailable';
      resyncAll.disabled = true;
      deleteAll.disabled = true;
      const retry = h('button', { class: 'btn btn-sm', type: 'button' }, 'Retry');
      retry.addEventListener('click', () => reload());
      const box = errorBlock(err);
      box.append(h('div', { class: 'models-retry' }, retry));
      tableWrap.replaceChildren(box);
      return;
    }

    // Rebuild the provider picker, keeping the operator's choice if it still exists. The first
    // option is always "all providers"; losing a selected provider should not silently show a
    // different provider's models under the old label.
    const previousProvider = provSel.value;
    provSel.replaceChildren(
      h('option', { value: '' }, txt('all providers (', providers.length, ')')),
      ...providers.map((p) => h('option', { value: p.id }, p.id)));
    provSel.value = providers.some((p) => p.id === previousProvider) ? previousProvider : '';

    updateSummary();
    renderTable();
    refreshDrawer();
  }

  // ------------------------------------------------------------------ events and first paint

  provSel.addEventListener('change', () => { page = 1; renderTable(null, true); });
  sourceSel.addEventListener('change', () => { page = 1; renderTable(null, true); });
  search.addEventListener('input', () => { page = 1; renderTable(null, true); });
  hiddenOnly.addEventListener('change', () => { page = 1; renderTable(null, true); });
  pageSizeSel.addEventListener('change', () => { page = 1; renderTable('size', true); });
  resyncAll.addEventListener('click', () => resyncAllModels());
  deleteAll.addEventListener('click', () => deleteAllModels());

  await reload();
}

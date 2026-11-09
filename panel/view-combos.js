// view-combos.js — combo list + editor.
//
// THE core product concept, taught in the UI: a combo's step list is the operator's priority
// order. docs/07-hedged-race.md: it "is never sorted, never re-weighted by the router... it is
// the operator's statement of preference, and the tie-breaker of last resort." Consequences in
// this file: the list renders in array order; reordering happens ONLY through moveStep() (drag
// or buttons), which relocates one element and leaves the rest untouched; nothing here ever
// calls .sort() on steps. Participants and the stable-fallback step are positional references,
// so they are remapped on every move (remapParticipants / remapIndex).
//
// All hedge help text below is a faithful summary of docs/07-hedged-race.md, including the
// counterintuitive part: the winner is the highest-priority participant that replied INSIDE the
// window — a lower-priority model answering faster does not win.

import { api } from './api.js';
import {
  h, icon, chip, field, toggle, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
} from './dom.js';
import {
  extractList, stepLabel, moveStep, remapParticipants, remapIndex,
  parseWinnerFail, buildWinnerFail, toggleParticipant,
  stepModel, stepProvider, modelLevels, providerLevels, probeTarget,
} from './format.js';
import { createSearchSelect } from './search-select.js';

// A check's result outlives the selection that produced it: an operator who checks three combos and
// then scrolls back should still see what came back. Keyed by combo id, then step index. Module
// scope, because the view can render a selected combo before its own body reaches a later `const`.
const checked = new Map();

const STRATEGIES = [
  ['priority', 'priority', 'Always start at step 1; on failure fall to step 2, and so on. Deterministic and sticky: the cheapest in upstream spend, and it keeps upstream prompt caches warm because the same model sees the conversation every turn.'],
  ['round-robin', 'round-robin', 'Each call starts at the next step (n mod len) and falls forward in priority order from there, wrapping. Rotation chooses the starting point only — after a failure the request still degrades down your order. The common use is rotating across accounts of one provider, whose per-account rate limits are independent.'],
  ['reset-aware-priority', 'reset-aware-priority', 'Priority order with one modification: any step with no available account is demoted below all healthy steps — demoted, not removed. It returns to the front automatically when its earliest known quota reset passes. Your order is preserved within each group.'],
];

const WINNER_METRICS = [
  ['first-response-byte', 'first response byte (TTFB)', 'Race is decided at the first byte of the winning response. The relay cancels local loser work, but providers may continue generating or billing after disconnect. For non-streaming upstreams the first body byte is the complete response, so hedging can cost a full completion per loser.'],
  ['first-complete-token', 'first complete token', 'Race is decided at the first fully decoded token. Fairer to providers that stream immediately and then think, but every loser pays one token.'],
];

const FAIL_MODES = [
  ['pick-highest-passed', 'use highest-priority that passed', 'Implemented. Do not re-dispatch: answer with the highest-priority participant that already passed the eligibility gate in the first competition. Zero added latency; the right mode when a fast, safe answer matters more than spending more upstream requests on recovery.'],
  ['rerun-excluding', 're-run excluding failed models', 'Implemented. Re-run the whole competition over the remaining participants, excluding every model that already failed, up to max_rounds. Costs extra upstream requests on recovery; max_rounds bounds it so one systemic outage cannot turn one request into unbounded spend.'],
  ['pick-highest-passed-different-provider', 'highest passed, different provider', 'Implemented. Answer with the highest-priority participant that passed, but only if it comes from a different provider than the one that died. Survives a whole-provider outage or quota reset that a same-provider fallback cannot.'],
  ['stable-fallback', 'jump to stable step', 'Implemented. On winner failure, skip the race and go straight to the chosen stable step — the operator-trusted model kept available for exactly this case.'],
];

export async function renderCombos(mount, ctx, selectedId) {
  let combos = [];
  let providers = [];

  const listEl = h('ul', { class: 'combo-list' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New combo');
  const count = h('span', { class: 'count' });
  const formSlot = h('div');
  const left = h('div', { class: 'card' }, h('h2', {}, 'Combos'), listEl);
  const right = h('div', { class: 'card' });
  // The editor currently mounted in the drawer, if any. It is cleared on close so an old editor's
  // dirty flag can never keep prompting after its DOM is gone. Declared before any immediate
  // renderEditor() call below, including the "#/combos/_new" route.
  let activeEditor = null;
  let closingEditor = false;
  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, h('span', { class: 'spacer' }), count),
    formSlot,
    h('div', { class: 'combo-layout' }, left, right),
  );

  newBtn.addEventListener('click', () => { void startNewEditor(); });

  try {
    // list_combos returns { combos: config.combos }; each ComboStep is only provider/model/policy
    // data. Reasoning capabilities belong to the provider's ModelEntry, so join them below rather
    // than pretending GET /api/combos contains compiled snapshot levels.
    combos = extractList(await api.get('/api/combos'), 'combos');
  } catch (err) {
    right.replaceChildren(errorBlock(err));
    return;
  }
  try { providers = await ctx.loadProviders(); } catch { /* combos remain usable without metadata */ }

  // A combo is called by its `name` (e.g. "deepseek"), but its stored id is a UUID. Links from the
  // sidebar and the overview use the name an operator recognises, so accept either rather than
  // sending a real combo to "Combo not found". Match the id first: a name that happens to look
  // like another combo's id should never win over the actual id.
  if (selectedId && selectedId !== '_new') {
    const selected = combos.find((c) => c.id === selectedId)
      ?? combos.find((c) => c.name === selectedId)
      ?? combos.find((c) => c.label === selectedId);
    selectedId = selected?.id ?? selectedId;
  }

  renderList();
  count.textContent = `${combos.length} combo${combos.length === 1 ? '' : 's'}`;

  if (selectedId === '_new') {
    renderEditor(null, formSlot);
    right.replaceChildren(emptyState('Create a combo.', 'Add a name and one or more steps to get started.'));
  } else if (selectedId) {
    const found = combos.find((c) => c.id === selectedId);
    if (found) renderSummary(right, found);
    else right.replaceChildren(emptyState('Combo not found.', 'It may have been deleted.'));
  } else {
    right.replaceChildren(combos.length
      ? emptyState('Select a combo on the left.', 'Or create a new one.')
      : emptyState('No combos yet.',
        'A combo is a named model: clients call it like any model, and it resolves to an ordered list of steps.'));
  }

  // ------------------------------------------------------------------ list

  function renderList() {
    listEl.replaceChildren();
    for (const c of combos) {
      const btn = h('button', { type: 'button', 'aria-current': c.id === selectedId ? 'true' : null },
        h('div', { class: 'cname' }, c.name || c.id),
        h('div', { class: 'cmeta' },
          `${(c.steps ?? []).length} step${(c.steps ?? []).length === 1 ? '' : 's'}`,
          chip(c.strategy ?? 'priority'),
          c.hedge?.enabled ? chip('hedged', 'chip-pink', 'hedged race enabled') : null,
          c.enabled === false ? chip('off') : null));
      btn.addEventListener('click', () => ctx.navigate(`#/combos/${encodeURIComponent(c.id)}`));
      listEl.append(h('li', {}, btn));
    }
  }

  // ------------------------------------------------------------------ detail drawer and summary

  function closeDrawer() {
    // Detach the editor and retract its guard together. A close that only removed the DOM would
    // leave ctx.leaveGuard pointing at a form that no longer exists, so the next page change would
    // ask about edits that are already gone.
    activeEditor?.discard();
    mount.querySelector('.drawer')?.remove();
    activeEditor = null;
  }

  // Ask before an editor is replaced, not only before the page changes. New combo and Edit combo
  // can both be clicked while a drawer is open; either one would otherwise unmount unsaved edits
  // silently and leave the old guard armed.
  async function confirmDiscardEditor(message) {
    const editor = activeEditor;
    if (!editor?.isDirty()) return true;
    if (closingEditor) return false;
    closingEditor = true;
    try {
      return await ctx.confirm({
        title: 'Leave without saving?',
        message,
        confirmLabel: 'Discard changes',
        cancelLabel: 'Keep editing',
        danger: false,
      });
    } finally {
      closingEditor = false;
    }
  }

  async function requestCloseDrawer() {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Closing the editor will discard them.'))) return;
    closeDrawer();
  }

  async function startNewEditor() {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Starting a new combo will discard them.'))) return;
    closeDrawer();
    renderEditor(null, formSlot);
  }

  function drawerShell(title) {
    closeDrawer();
    const head = h('div', { class: 'drawer-head' },
      h('h2', {}, title),
      h('span', { class: 'spacer' }));
    const body = h('div', { class: 'drawer-body' });
    const drawer = h('div', { class: 'drawer', role: 'region', 'aria-label': 'Combo detail' }, head, body);
    drawer.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') { e.preventDefault(); void requestCloseDrawer(); }
    });
    mount.append(drawer);
    return { drawer, head, body };
  }

  async function openEditorDrawer(saved) {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Opening another combo will discard them.'))) return;
    const { drawer, head, body } = drawerShell(saved ? `Edit combo ${saved.name || saved.id}` : 'New combo');
    head.append(h('button', {
      class: 'icon-btn', type: 'button', 'aria-label': 'Close combo editor',
      onclick: () => { void requestCloseDrawer(); },
    }, icon('x')));
    renderEditor(saved, body);
    // A new editor focuses its name field. An existing one has no natural first input, so put
    // focus on the drawer itself rather than leaving it on the summary button behind it — Escape
    // must close this editor, and a focused element outside the drawer never reaches its handler.
    if (!body.contains(document.activeElement)) {
      drawer.tabIndex = -1;
      drawer.focus({ preventScroll: true });
    }
  }

  function renderSummary(target, combo) {
    const healthSection = h('section', {});
    const editBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'Edit combo');
    editBtn.addEventListener('click', () => { void openEditorDrawer(combo); });
    target.replaceChildren(
      h('h2', {}, combo.label || combo.name || combo.id),
      h('p', { class: 'hint mono' }, `id: ${combo.id}`),
      h('p', { class: 'hint' }, combo.name || combo.id),
      h('div', { class: 'cmeta' },
        chip(`strategy: ${combo.strategy ?? 'priority'}`),
        combo.hedge?.enabled ? chip('hedged', 'chip-pink', 'hedged race enabled') : null,
        combo.enabled === false ? chip('off') : chip('enabled', 'chip-green')),
      h('section', {},
        h('h3', {}, 'Steps — priority order'),
        h('ol', { class: 'steps' }, ...(combo.steps ?? []).map((step, i) => {
          // A combo target has no model of its own to pin a level on: the nested combo supplies
          // its steps' levels. Showing a ladder here would offer a control with no routing effect.
          const comboStep = Boolean(step.combo);
          const model = comboStep ? null : stepModel(step, providers);
          const levelsForStep = comboStep ? [] : modelLevels(model, stepProvider(step, providers));
          const defaultLevel = model?.default_reasoning_level;
          const offersLevel = defaultLevel && levelsForStep.includes(defaultLevel);
          return h('li', { class: 'step-row' },
            h('span', { class: 'step-pos' }, String(i + 1)),
            h('span', { class: 'step-ref mono' }, stepLabel(step)),
            !comboStep && step.level ? chip(`at ${step.level}`, 'chip-neutral', 'reasoning level this step runs at') : null,
            ...levelsForStep.map((level) => chip(String(level), 'chip-neutral', 'Reasoning level offered by this model')),
            defaultLevel && !offersLevel ? chip(`default ${defaultLevel}`, 'chip-pink', 'Model-specific default reasoning level') : null,
            step.enabled === false ? chip('off') : null);
        }))),
      healthSection,
      h('div', { class: 'dialog-actions' }, editBtn),
    );
    renderHealth(healthSection, combo);
  }

  /** One step's outcome, as the checker left it. */
  function stepStatus(comboId, index) {
    return checked.get(comboId)?.get(index);
  }

  function statusChip(status) {
    if (!status) return chip('not checked', 'chip-neutral', 'never probed in this session');
    if (status.state === 'checking') return chip('checking…', 'chip-neutral');
    if (status.state === 'ok') return chip(`ok · ${status.ms} ms`, 'chip-green', 'the model answered');
    return chip(`failed · ${status.code ?? status.status}`, 'chip-pink', status.message ?? 'the model did not answer');
  }

  // A check is one tiny completion per model step, in priority order, and it is always the
  // operator's click: nothing here runs on page load or on a timer. A step that routes to another
  // combo has no provider/model pair of its own, so it is listed but not probed; the nested combo
  // carries the model steps that can answer.
  function renderHealth(section, combo) {
    const steps = (combo.steps ?? []).map((step, index) => ({ step, index }));
    const checkable = steps.flatMap(({ step, index }) => {
      const target = step.enabled === false ? null : probeTarget(step);
      return target ? [{ step, index, target }] : [];
    });
    const run = h('button', { class: 'btn btn-small', type: 'button' }, 'Check steps');
    const out = h('div', {});

    const paint = () => {
      const results = checked.get(combo.id) ?? new Map();
      const answered = checkable.filter(({ index }) => results.get(index)?.state === 'ok');
      const failed = checkable.filter(({ index }) => results.get(index)?.state === 'failed');
      out.replaceChildren(
        h('ol', { class: 'steps' }, ...steps.map(({ step, index }) => {
          const status = results.get(index);
          return h('li', { class: 'step-row' },
            h('span', { class: 'step-pos' }, String(index + 1)),
            h('span', { class: 'step-ref mono' }, stepLabel(step)),
            !step.combo && step.level ? chip(`at ${step.level}`, 'chip-neutral', 'pinned reasoning level') : null,
            step.combo
              ? chip('nested combo', 'chip-neutral', 'this step runs another combo\u2019s steps; open that combo to check them')
              : statusChip(status),
            status?.message && status.state === 'failed'
              ? h('span', { class: 'hint' }, status.message)
              : null);
        })),
        checkable.length && results.size
          ? h('p', { class: 'hint' },
            answered.length
              ? `${answered.length} of ${checkable.length} answering${failed.length ? `, ${failed.length} failing` : ''}.`
              : `Nothing answered${failed.length ? `: ${failed.map(({ index }) => `step ${index + 1}`).join(', ')}` : ''}.`)
          : h('p', { class: 'hint' },
            checkable.length
              ? 'Each check sends one trivial completion per enabled model step, in priority order; a step that routes to another combo is not probed here.'
              : 'No enabled model steps to check; a step that routes to another combo is checked in that combo.'));
    };

    run.addEventListener('click', async () => {
      if (run.disabled) return;
      run.disabled = true;
      const results = new Map();
      checked.set(combo.id, results);
      try {
        for (const { index, target } of checkable) {
          results.set(index, { state: 'checking' });
          paint();
          const started = performance.now();
          try {
            // A step's own 401 is a provider refusal, not a dead panel session: report it in this
            // row instead of signing the operator out mid-check.
            const answer = await api.post('/api/models/test', {
              provider: target.provider,
              model: target.model,
              prompt: 'Reply with the single word OK.',
            }, { skipAuthRedirect: true });
            results.set(index, {
              state: 'ok',
              ms: Math.round(performance.now() - started),
              answer: answer?.response ?? null,
            });
          } catch (err) {
            results.set(index, {
              state: 'failed',
              status: err?.status ?? 'error',
              code: err?.code ?? 'failed',
              message: err?.message ?? 'the request failed',
            });
          }
          paint();
        }
      } finally {
        run.disabled = false;
      }
    });

    paint();
    section.replaceChildren(
      h('div', { class: 'cmeta' }, h('h3', {}, 'Live health'), run),
      out);
  }

  // ------------------------------------------------------------------ editor

  function renderEditor(saved, target = right) {
    const isNew = saved === null;
    // Draft is a deep copy: edits are local until Save; Revert re-reads from `combos`.
    const draft = isNew ? newDraft() : JSON.parse(JSON.stringify(saved));
    // Normalise the hedge object defensively: a combo saved without one must not crash the
    // editor, and the defaults here mirror the backend's (count 3, window 800 ms, TTFB metric).
    draft.hedge = {
      enabled: draft.hedge?.enabled ?? false,
      sticky_success_window_secs: draft.sticky_success_window_secs ?? 1800,
      participants: draft.hedge?.participants ?? [],
      count: draft.hedge?.count ?? 3,
      window_ms: draft.hedge?.window_ms ?? 800,
      winner_metric: draft.hedge?.winner_metric ?? 'first-response-byte',
      on_winner_fail: draft.hedge?.on_winner_fail ?? { 'pick-highest-passed': null },
      cancel_losers: draft.hedge?.cancel_losers ?? true,
      loser_grace_ms: draft.hedge?.loser_grace_ms ?? 0,
    };
    draft.wf = parseWinnerFail(draft.hedge.on_winner_fail); // working form of the tagged enum
    const savedSnapshot = JSON.stringify(draft);
    let dirty = false;
    let guard = null;

    function markDirty() {
      if (dirty) return;
      dirty = true;
      guard = () => ctx.confirm({
        title: 'Leave without saving?',
        message: 'This combo has unsaved changes. Leaving will discard them.',
        confirmLabel: 'Leave and discard',
        cancelLabel: 'Stay on this page',
        danger: false,
      });
      ctx.leaveGuard = guard;
    }
    function clearDirty() {
      dirty = false;
      // Only retract our own guard: a late clear from an old editor must not disarm a newer one.
      if (guard && ctx.leaveGuard === guard) ctx.leaveGuard = null;
      guard = null;
    }
    // The drawer owns this editor's lifetime. The router clears the guard after a confirmed page
    // change, but closing the drawer stays on the same route and must clean up on its own.
    activeEditor = { isDirty: () => dirty, discard: clearDirty };

    const title = h('h2', { style: 'text-transform:none;letter-spacing:0;font-size:14px;' },
      isNew ? 'New combo' : `Editing ${saved.name || saved.id}`);
    const nameIn = h('input', { class: 'input mono', type: 'text', 'data-field': 'name', value: draft.name ?? '', placeholder: 'callable model name, e.g. Brains' });
    const labelIn = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: draft.label ?? '' });

    const stratSel = h('select', { class: 'input', 'data-field': 'strategy' });
    for (const [v, lbl] of STRATEGIES) stratSel.append(h('option', { value: v, selected: (draft.strategy ?? 'priority') === v }, lbl));
    const stratHint = h('p', { class: 'hint' });
    stratSel.addEventListener('change', () => { updateStrategyHint(); markDirty(); });

    // --- steps ---
    const stepsOl = h('ol', { class: 'steps' });
    const stepCallout = h('div', { class: 'callout' },
      h('strong', {}, 'This order is your priority order. '),
      'The panel and the router never sort, re-weight or second-guess it: step 1 is tried first, ',
      'and in a hedged race the winner is the highest-priority participant that replied within the window. ',
      'Reorder by dragging a row (or the arrow buttons) — the number on the left is the rank.');

    // Three pickers, one row. A step is either provider/model or a route to another combo; the
    // small selector swaps which ones are visible so the common case keeps two fields.
    const addKind = h('select', { class: 'input add-kind', 'aria-label': 'What this step routes to' },
      h('option', { value: 'model' }, 'model'),
      h('option', { value: 'combo' }, 'combo'));
    const addProv = createSearchSelect({
      ariaLabel: 'Provider for new step',
      placeholder: 'provider…',
      emptyText: 'no provider matches',
      onChoose: () => syncAddModels(),
    });
    const addModel = createSearchSelect({
      ariaLabel: 'Model for new step',
      placeholder: 'model id',
      emptyText: 'no model matches',
    });
    const addCombo = createSearchSelect({
      ariaLabel: 'Combo this step routes to',
      placeholder: 'combo name…',
      emptyText: 'no combo matches',
    });
    addCombo.el.hidden = true;
    const addNote = h('input', { class: 'input', type: 'text', 'aria-label': 'Note for new step (optional)', placeholder: 'note (optional)' });
    const addBtn = h('button', { class: 'btn', type: 'button' }, 'Add step');
    const addRow = h('div', { class: 'add-step-row' },
      addKind, addProv.el, addModel.el, addCombo.el, addNote, addBtn);

    function providerChoices() {
      return (providers ?? []).map((p) => ({
        value: p.id,
        label: p.id,
        hint: p.label && p.label !== p.id ? p.label : undefined,
      }));
    }
    function modelChoices(providerId) {
      const provider = (providers ?? []).find((p) => p.id === providerId);
      return (provider?.models ?? []).map((m) => ({
        value: m.id,
        label: m.id,
        hint: m.label && m.label !== m.id ? m.label : undefined,
      }));
    }
    function comboChoices() {
      return combos
        .filter((c) => c.id !== draft.id && c.name !== draft.name)
        .map((c) => ({
          value: c.name || c.id,
          label: c.name || c.id,
          hint: c.name && c.name !== c.id ? c.id : undefined,
        }));
    }
    function syncAddModels() {
      addModel.setOptions(modelChoices(addProv.value()));
    }
    function syncAddKind() {
      const toCombo = addKind.value === 'combo';
      addProv.el.hidden = toCombo;
      addModel.el.hidden = toCombo;
      addCombo.el.hidden = !toCombo;
    }
    addKind.addEventListener('change', syncAddKind);

    addProv.setOptions(providerChoices());
    addCombo.setOptions(comboChoices());
    ctx.loadProviders().then((next) => {
      providers = next ?? providers;
      addProv.setOptions(providerChoices());
      addCombo.setOptions(comboChoices());
      syncAddModels();
    }).catch(() => { /* the editor still works with what was loaded */ });
    syncAddModels();

    addBtn.addEventListener('click', () => {
      const note = addNote.value.trim() || undefined;
      if (addKind.value === 'combo') {
        const combo = addCombo.value();
        if (!combo) { ctx.toast.warning('pick a combo for the step'); return; }
        if (combo === draft.name || combo === draft.id) {
          ctx.toast.warning('a combo cannot route to itself');
          return;
        }
        // Appends at the END: a new step starts at the lowest priority. It is never inserted
        // "where it fits" — only the operator decides placement.
        draft.steps.push({ combo, enabled: true, note });
        addCombo.setValue(''); addNote.value = '';
      } else {
        const provider = addProv.value();
        const model = addModel.value();
        const problems = [];
        if (!provider) problems.push('pick a provider for the step');
        else if (!(providers ?? []).some((p) => p.id === provider)) problems.push('pick a provider from the list');
        if (!model) problems.push('enter a model id for the step');
        if (problems.length) { ctx.toast.warning(problems.join(' — ')); return; }
        draft.steps.push({ provider, model, enabled: true, note });
        addModel.setValue(''); addNote.value = '';
      }
      renderSteps(); renderStableSelect(); renderParticipantSummary();
      markDirty();
    });

    // --- hedge ---
    const hedgeToggle = toggle('Enable the hedged race for this combo', { checked: Boolean(draft.hedge?.enabled) });
    const hedgeOn = hedgeToggle.input;
    const stickyWindowInput = h('input', { class: 'input', type: 'number', min: '0', max: '604800', value: draft.sticky_success_window_secs ?? 1800 });
    stickyWindowInput.addEventListener('input', () => markDirty());
    const stickyWindowBox = field('Sticky success window (seconds)', stickyWindowInput, { hint: 'Priority and reset-aware combos. Default 1800s (30 minutes). Combo-wide across callers; inactivity refreshes after a successful response. Set 0 to disable.' });
    const hedgeCount = h('input', { class: 'input', type: 'number', min: '2', max: '255', 'data-field': 'count', value: draft.hedge?.count ?? 3 });
    hedgeCount.addEventListener('input', () => markDirty());
    const hedgeWindow = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'window_ms', value: draft.hedge?.window_ms ?? 800 });
    hedgeWindow.addEventListener('input', () => markDirty());
    const metricSel = h('select', { class: 'input', 'data-field': 'winner_metric' });
    for (const [v, lbl] of WINNER_METRICS) metricSel.append(h('option', { value: v, selected: (draft.hedge?.winner_metric ?? 'first-response-byte') === v }, lbl));
    const metricHint = h('p', { class: 'hint' });
    metricSel.addEventListener('change', () => { updateMetricHint(); markDirty(); });

    const failSel = h('select', { class: 'input', 'data-field': 'on_winner_fail' });
    for (const [v, lbl] of FAIL_MODES) failSel.append(h('option', { value: v, selected: draft.wf.mode === v }, lbl));
    const failHint = h('p', { class: 'hint' });
    const maxRounds = h('input', { class: 'input', type: 'number', min: '1', max: '255', 'data-field': 'max_rounds', value: draft.wf.maxRounds ?? 2 });
    maxRounds.addEventListener('input', () => markDirty());
    const stableSel = h('select', { class: 'input', 'data-field': 'step' });
    const maxRoundsField = field('Max rounds', maxRounds, { hint: 'Bound for re-run excluding.' });
    const stableField = field('Stable step', stableSel, { hint: 'The on-fail model; need not be a race participant.' });
    failSel.addEventListener('change', () => { draft.wf.mode = failSel.value; renderFailExtras(); updateFailHint(); markDirty(); });
    stableSel.addEventListener('change', () => { draft.wf.step = Number(stableSel.value); markDirty(); });
    const cancelLosersToggle = toggle('Cancel losing requests once the winner is chosen', { checked: draft.hedge?.cancel_losers ?? true });
    const cancelLosers = cancelLosersToggle.input;
    cancelLosers.addEventListener('change', () => markDirty());
    const loserGrace = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'loser_grace_ms', value: draft.hedge?.loser_grace_ms ?? 0 });
    loserGrace.addEventListener('input', () => markDirty());
    const participantSummary = h('p', { class: 'hint' });

    function renderStableSelect() {
      stableSel.replaceChildren();
      draft.steps.forEach((s, i) => stableSel.append(h('option', { value: String(i), selected: draft.wf.step === i }, `#${i + 1} ${stepLabel(s)}`)));
      stableSel.value = String(draft.wf.step ?? 0);
    }
    function renderFailExtras() {
      maxRoundsField.style.display = draft.wf.mode === 'rerun-excluding' ? '' : 'none';
      stableField.style.display = draft.wf.mode === 'stable-fallback' ? '' : 'none';
    }
    function renderParticipantSummary() {
      const n = (draft.hedge?.participants ?? []).length;
      participantSummary.textContent = n
        ? `${n} step${n === 1 ? '' : 's'} ticked as race participants. Untick all to race the first “count” enabled steps instead.`
        : 'No explicit participants: the first “count” enabled steps enter the race.';
    }

    const hedgeBox = h('fieldset', { class: 'hedge' },
      h('legend', {}, 'Hedged race and sticky success'),
      stickyWindowBox,
      hedgeToggle.row,
      h('p', { class: 'hint', style: 'margin-bottom:8px;' },
        'The window is an eligibility gate, not a stopwatch race: the winner is the highest-priority participant that produced a valid reply within the window. A lower-priority model answering faster does not win; if nobody answers in time, nothing is re-spent.'),
      h('div', { class: 'grid-fields' },
        field('Count', hedgeCount, { hint: 'Participants when none are ticked per-step.' }),
        field('Window (ms)', hedgeWindow, { hint: 'How long higher-priority steps are protected.' })),
      field('Winner metric', metricSel, { hint: null }),
      metricHint,
      field('On winner fail', failSel),
      failHint,
      h('div', { class: 'grid-fields' }, maxRoundsField, stableField),
      cancelLosersToggle.row,
      h('div', { class: 'grid-fields' },
        field('Loser grace (ms)', loserGrace, { hint: '0 = cancel immediately.' })),
      participantSummary,
    );
    hedgeOn.addEventListener('change', () => { hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; stickyWindowBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; markDirty(); });
    hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55';
    renderStableSelect(); renderFailExtras(); renderParticipantSummary();
    updateStrategyHint(); updateMetricHint(); updateFailHint();

    // --- delivery ---
    const midSel = h('select', { class: 'input', 'data-field': 'midstream_policy' },
      h('option', { value: 'restart', selected: (draft.midstream_policy ?? 'restart') === 'restart' }, 'restart'),
      h('option', { value: 'terminate', selected: draft.midstream_policy === 'terminate' }, 'terminate'));
    midSel.addEventListener('change', () => markDirty());
    const maxRetries = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'max_retries', value: draft.max_retries ?? 1 });
    maxRetries.addEventListener('input', () => markDirty());
    const retryDelay = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'retry_delay_ms', value: draft.retry_delay_ms ?? 0 });
    retryDelay.addEventListener('input', () => markDirty());
    const affinityToggle = toggle('Merge identical consecutive messages when translating between dialects', { checked: Boolean(draft.affinity) });
    const affinity = affinityToggle.input;
    affinity.addEventListener('change', () => markDirty());
    const comboEnabledToggle = toggle('Combo enabled', { checked: draft.enabled ?? true });
    const comboEnabled = comboEnabledToggle.input;
    comboEnabled.addEventListener('change', () => markDirty());

    // --- actions ---
    const saveBtn = h('button', { class: 'btn btn-primary', type: 'button' }, isNew ? 'Create combo' : 'Save combo');
    const delBtn = h('button', { class: 'btn btn-danger', type: 'button', disabled: isNew }, 'Delete combo');
    const revertBtn = h('button', { class: 'btn', type: 'button', disabled: isNew }, 'Revert');

    saveBtn.addEventListener('click', async () => {
      clearFieldErrors(editor);
      // Keep only structural checks here. Model ids are free text on purpose: the server resolves
      // provider/model names and its field errors explain typos better than stale client metadata.
      const problems = [];
      if (!draft.name?.trim()) problems.push({ field: 'name', message: 'the callable name is required — clients will pass it as the model' });
      if (!draft.steps.length) problems.push({ field: 'steps', message: 'a combo needs at least one step' });
      // A combo target is a complete step without provider or model; a model step still needs both.
      if (draft.steps.some((s) => (s.combo ? !String(s.combo).trim() : (!s.provider || !String(s.model ?? '').trim())))) {
        problems.push({ field: 'steps', message: 'every step needs a provider and a model id, or a combo' });
      }
      if (draft.wf.mode === 'stable-fallback' && !(draft.wf.step >= 0 && draft.wf.step < draft.steps.length)) {
        problems.push({ field: 'step', message: 'the stable step must point at one of this combo\u2019s steps' });
      }
      if (problems.length) { applyFieldErrors(editor, { message: 'Fix these before saving.', details: problems }); return; }

      saveBtn.disabled = true;
      saveBtn.textContent = 'Saving…';
      // Rebuild the tagged enum in its wire shape; participants are step indices.
      const payload = {
        ...draft,
        name: draft.name.trim(),
        // An empty field means "leave it at the default", not "switch it off". Reading it as 0
        // silently disabled stickiness on every combo saved through this form.
        sticky_success_window_secs: stickyWindowInput.value.trim() === '' ? 1800 : (Number(stickyWindowInput.value) || 0),
        steps: draft.steps.map((s) => {
          const step = { ...s, note: s.note || undefined };
          // A combo target ignores provider, model and pinned level at compile time. Strip them
          // rather than write fields that look meaningful but are never read.
          if (step.combo) {
            delete step.provider;
            delete step.model;
            delete step.level;
          }
          return step;
        }),
        hedge: {
          enabled: hedgeOn.checked,
          participants: draft.hedge.participants ?? [],
          count: Number(hedgeCount.value) || 3,
          window_ms: Number(hedgeWindow.value) || 0,
          winner_metric: metricSel.value,
          on_winner_fail: buildWinnerFail(draft.wf.mode, { maxRounds: Number(maxRounds.value), step: Number(stableSel.value) }),
          cancel_losers: cancelLosers.checked,
          loser_grace_ms: Number(loserGrace.value) || 0,
        },
        strategy: stratSel.value,
        midstream_policy: midSel.value,
        max_retries: Number(maxRetries.value) || 0,
        retry_delay_ms: Number(retryDelay.value) || 0,
        affinity: affinity.checked,
        enabled: comboEnabled.checked,
      };
      delete payload.wf; // panel-internal working state, never sent

      try {
        if (isNew) {
          const created = await api.post('/api/combos', payload);
          ctx.toast.success(`Combo ${payload.name} created.`);
          clearDirty();
          closeDrawer();
          ctx.navigate(`#/combos/${encodeURIComponent(created?.id ?? payload.name)}`);
        } else {
          await api.put(`/api/combos/${encodeURIComponent(saved.id)}`, payload);
          ctx.toast.success(`Combo ${payload.name} saved.`);
          clearDirty();
          closeDrawer();
          ctx.navigate('#/combos');
        }
      } catch (err) {
        applyFieldErrors(editor, err); // every backend problem, listed and pinned to fields
        if (!err.details?.length) ctx.toast.error('Could not save the combo', err);
        saveBtn.disabled = false;
        saveBtn.textContent = isNew ? 'Create combo' : 'Save combo';
      }
    });

    delBtn.addEventListener('click', async () => {
      const ok = await ctx.confirm({
        title: `Delete combo “${saved.name || saved.id}”?`,
        message: 'Clients calling this model name will get “model not found”.',
        confirmLabel: 'Delete combo',
      });
      if (!ok) return;
      try {
        await api.del(`/api/combos/${encodeURIComponent(saved.id)}`);
        ctx.toast.success('Combo deleted.');
        // The record is gone, so its editor must not ask to preserve unsaved edits on the way out.
        clearDirty();
        closeDrawer();
        ctx.navigate('#/combos');
      } catch (err) {
        ctx.toast.error('Delete failed', err);
      }
    });

    // Re-render in the same slot: an existing combo's editor lives in the drawer, and the inline
    // right-hand slot would leave the old, still-dirty drawer mounted beside a second form.
    revertBtn.addEventListener('click', () => { clearDirty(); renderEditor(saved, target); });

    const editor = h('form', { class: 'combo-editor', onsubmit: (e) => e.preventDefault() },
      title,
      isNew ? null : h('p', { class: 'hint mono', style: 'margin-top:-4px;' }, `id: ${saved.id}`),
      h('div', { class: 'grid-fields' },
        field('Name — the model clients call', nameIn, { hint: 'A combo is a named model: there is no special endpoint or syntax.' }),
        field('Label (display only)', labelIn)),
      field('Strategy', stratSel),
      stratHint,
      h('fieldset', { class: 'plain' },
        h('legend', {}, 'Steps — priority order, never sorted'),
        stepCallout,
        stepsOl,
        addRow),
      hedgeBox,
      h('fieldset', { class: 'plain' },
        h('legend', {}, 'Delivery'),
        field('Midstream policy', midSel, { hint: 'restart: if the winner disconnects mid-stream, restart the request from the beginning at an SSE event boundary (the client sees a restart marker). terminate: end the request. Both refuse to splice mid tool_calls.' }),
        h('div', { class: 'grid-fields' },
          field('Max retries', maxRetries, { hint: 'Retries of a failing step before falling to the next one.' }),
          field('Retry delay (ms)', retryDelay)),
        affinityToggle.row,
        comboEnabledToggle.row),
      h('div', { class: 'dialog-actions' }, revertBtn, delBtn, saveBtn),
    );

    nameIn.addEventListener('input', () => { draft.name = nameIn.value; markDirty(); });
    labelIn.addEventListener('input', () => { draft.label = labelIn.value; markDirty(); });

    target.replaceChildren(editor);
    renderSteps();
    if (isNew) nameIn.focus();

    // ------------------------------------------------------------------
    // Steps rendering & reorder mechanics

    function renderSteps() {
      stepsOl.replaceChildren();
      if (!draft.steps.length) {
        stepsOl.append(h('li', {}, emptyState('No steps yet.', 'Add the first step below — it becomes priority #1.')));
        return;
      }
      draft.steps.forEach((step, i) => {
        const pos = h('span', { class: 'step-pos' }, String(i + 1));
        const ref = h('span', { class: 'step-ref mono' }, stepLabel(step));

        const race = h('input', {
          type: 'checkbox', 'aria-label': `Step ${i + 1} races (hedged participant)`,
        });
        race.checked = (draft.hedge.participants ?? []).includes(i);
        race.addEventListener('change', () => {
          draft.hedge.participants = toggleParticipant(draft.hedge.participants, i);
          renderParticipantSummary();
          markDirty();
        });

        const on = h('input', { type: 'checkbox', 'aria-label': `Step ${i + 1} enabled` });
        on.checked = step.enabled !== false;
        on.addEventListener('change', () => { draft.steps[i].enabled = on.checked; markDirty(); });

        const up = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Move step ${i + 1} up` }, icon('up'));
        const down = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Move step ${i + 1} down` }, icon('down'));
        up.disabled = i === 0;
        down.disabled = i === draft.steps.length - 1;
        up.addEventListener('click', () => doMove(i, i - 1));
        down.addEventListener('click', () => doMove(i, i + 1));

        // A model step may run at its own tier. A combo target has no model to pin a level on:
        // the nested combo supplies its own steps' levels, so there is no selector to offer.
        let levelSelect = null;
        if (!step.combo) {
          // The list is the model's effective levels — declared on the entry, otherwise derived
          // from the provider dialect and capped by what that dialect can send. Offering a level
          // the model does not offer is how a request becomes a silent substitution; a level
          // already configured but no longer offered is kept and labelled with the ceiling it will
          // actually run at, so the value is never silently rewritten.
          const stepMeta = stepModel(step, providers);
          const stepProviderEntry = stepProvider(step, providers);
          const offered = modelLevels(stepMeta, stepProviderEntry);
          const ceiling = offered[offered.length - 1];
          // Every level the provider's dialect can put on the wire is selectable, so a level can
          // be pinned by hand even when the model entry advertises none (a model synced without
          // declared levels used to leave "model default" as the only choice). A level the model
          // does not offer is still honest: the label says what it will actually run at, which is
          // the ceiling the relay substitutes.
          const sendable = providerLevels(stepProviderEntry, stepMeta);
          const levelOptions = [h('option', { value: '' }, 'model default')];
          for (const level of sendable) {
            levelOptions.push(h('option', {
              value: level,
              selected: step.level === level,
            }, offered.includes(level) ? level : `${level} → ${ceiling ?? 'no reasoning'}`));
          }
          if (step.level && !sendable.includes(step.level)) {
            levelOptions.push(h('option', {
              value: step.level, selected: true,
            }, `${step.level} → unsendable on this provider`));
          }
          levelSelect = h('select', {
            class: 'input step-level',
            'aria-label': `Reasoning level for step ${i + 1}`,
            title: 'Reasoning level this step runs at',
          }, ...levelOptions);
          levelSelect.addEventListener('change', () => {
            if (levelSelect.value) draft.steps[i].level = levelSelect.value;
            else delete draft.steps[i].level;
            markDirty();
          });
        }

        const edit = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Edit step ${i + 1}` }, icon('pencil'));
        edit.addEventListener('click', () => renderStepEdit(i));

        const del = h('button', { class: 'icon-btn danger', type: 'button', 'aria-label': `Remove step ${i + 1}` }, icon('x'));
        del.addEventListener('click', () => {
          draft.steps.splice(i, 1);
          // Removing a step shifts every later index: remap positional references.
          draft.hedge.participants = (draft.hedge.participants ?? [])
            .filter((p) => p !== i).map((p) => (p > i ? p - 1 : p));
          if (draft.wf.step === i) draft.wf.step = 0;
          else if (draft.wf.step > i) draft.wf.step -= 1;
          renderSteps(); renderStableSelect(); renderParticipantSummary();
        });

        const li = h('li', { class: 'step-row', draggable: 'true', 'data-idx': String(i), tabindex: '-1' },
          pos,
          h('span', { class: 'icon-btn', 'aria-hidden': 'true', title: 'Drag to reorder (or use the arrows)' }, icon('grip')),
          ref,
          levelSelect,
          h('label', { class: 'step-flag' }, race, 'race'),
          h('label', { class: 'step-flag' }, on, 'on'),
          up, down, edit, del);

        li.addEventListener('dragstart', (e) => {
          e.dataTransfer.setData('text/plain', String(i));
          e.dataTransfer.effectAllowed = 'move';
          li.classList.add('dragging');
        });
        li.addEventListener('dragend', () => li.classList.remove('dragging'));
        li.addEventListener('dragover', (e) => { e.preventDefault(); li.classList.add('drag-over'); });
        li.addEventListener('dragleave', () => li.classList.remove('drag-over'));
        li.addEventListener('drop', (e) => {
          e.preventDefault();
          li.classList.remove('drag-over');
          const raw = e.dataTransfer.getData('text/plain');
          if (!/^\d+$/.test(raw)) return;
          const from = Number(raw);
          if (Number.isInteger(from)) doMove(from, i);
        });

        stepsOl.append(li);
      });
    }

    function renderStepEdit(i) {
      const li = stepsOl.querySelector(`li[data-idx="${i}"]`);
      if (!li) return;
      const step = draft.steps[i];
      const kindSel = h('select', { class: 'input', 'aria-label': `What step ${i + 1} routes to` },
        h('option', { value: 'model', selected: !step.combo }, 'model'),
        h('option', { value: 'combo', selected: Boolean(step.combo) }, 'combo'));
      const provSel = createSearchSelect({
        ariaLabel: `Provider for step ${i + 1}`,
        placeholder: 'provider…',
        value: step.provider ?? '',
        emptyText: 'no provider matches',
        onChoose: () => fillModels(),
      });
      const modelSel = createSearchSelect({
        ariaLabel: `Model for step ${i + 1}`,
        placeholder: 'model id',
        value: step.model ?? '',
        emptyText: 'no model matches',
      });
      const comboPick = createSearchSelect({
        ariaLabel: `Combo for step ${i + 1}`,
        placeholder: 'combo name…',
        value: step.combo ?? '',
        emptyText: 'no combo matches',
      });
      const noteIn = h('input', { class: 'input', type: 'text', value: step.note ?? '', placeholder: 'note (optional)', 'aria-label': `Note for step ${i + 1}` });
      const done = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Done');

      function localProviders(next) {
        return next !== undefined ? next : (ctx.state.providers ?? providers ?? []);
      }
      function fillModels() {
        const provider = localProviders().find((p) => p.id === provSel.value());
        modelSel.setOptions((provider?.models ?? []).map((m) => ({
          value: m.id,
          label: m.id,
          hint: m.label && m.label !== m.id ? m.label : undefined,
        })));
      }
      function applyKind() {
        const toCombo = kindSel.value === 'combo';
        provSel.el.hidden = toCombo;
        modelSel.el.hidden = toCombo;
        comboPick.el.hidden = !toCombo;
      }
      function fillProviderChoices(next) {
        provSel.setOptions(localProviders(next).map((p) => ({
          value: p.id,
          label: p.id,
          hint: p.label && p.label !== p.id ? p.label : undefined,
        })));
      }
      kindSel.addEventListener('change', applyKind);
      provSel.setOptions(localProviders().map((p) => ({
        value: p.id,
        label: p.id,
        hint: p.label && p.label !== p.id ? p.label : undefined,
      })));
      comboPick.setOptions(combos
        .filter((c) => c.id !== draft.id && c.name !== draft.name)
        .map((c) => ({ value: c.name || c.id, label: c.name || c.id, hint: c.name && c.name !== c.id ? c.id : undefined })));
      fillModels();
      applyKind();

      ctx.loadProviders().then((next) => {
        providers = next ?? providers;
        fillProviderChoices(next);
        fillModels();
      }).catch(() => {});

      done.addEventListener('click', () => {
        const note = noteIn.value.trim() || undefined;
        if (kindSel.value === 'combo') {
          const combo = comboPick.value();
          if (!combo) { ctx.toast.warning('pick a combo for the step'); return; }
          if (combo === draft.name || combo === draft.id) {
            ctx.toast.warning('a combo cannot route to itself');
            return;
          }
          draft.steps[i] = { combo, enabled: step.enabled !== false, note };
        } else {
          const provider = provSel.value();
          const model = modelSel.value();
          if (!provider) { ctx.toast.warning('pick a provider for the step'); return; }
          if (!model) { ctx.toast.warning('enter a model id for the step'); return; }
          const next = { ...step, provider, model, note };
          delete next.combo;
          draft.steps[i] = next;
        }
        renderSteps(); renderStableSelect();
        markDirty();
      });
      li.replaceChildren(h('div', { class: 'step-edit' },
        h('span', { class: 'step-pos' }, String(i + 1)), kindSel, provSel.el, modelSel.el, comboPick.el, noteIn, done));
      (kindSel.value === 'combo' ? comboPick : modelSel).input.focus();
    }

    // The ONLY way order changes. After the re-render the moved row is focused so keyboard
    // users keep their place in the list.
    function doMove(from, to) {
      if (!Number.isInteger(from) || from < 0 || from >= draft.steps.length) return;
      if (to < 0 || to >= draft.steps.length || from === to) return;
      draft.steps = moveStep(draft.steps, from, to);
      // Positional references move with the rows — forgetting this would silently re-point
      // the hedged race at the wrong models after a reorder.
      draft.hedge.participants = remapParticipants(draft.hedge.participants, from, to);
      draft.wf.step = remapIndex(draft.wf.step, from, to);
      renderSteps(); renderStableSelect(); renderParticipantSummary();
      stepsOl.querySelector(`li[data-idx="${to}"]`)?.focus();
      markDirty();
    }

    function updateStrategyHint() { stratHint.textContent = STRATEGIES.find(([v]) => v === stratSel.value)?.[2] ?? ''; }
    function updateMetricHint() { metricHint.textContent = WINNER_METRICS.find(([v]) => v === metricSel.value)?.[2] ?? ''; }
    function updateFailHint() { failHint.textContent = FAIL_MODES.find(([v]) => v === draft.wf.mode)?.[2] ?? ''; }
  }

  function newDraft() {
    return {
      name: '',
      label: '',
      steps: [],
      strategy: 'priority',
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
      midstream_policy: 'restart',
      max_retries: 1,
      retry_delay_ms: 0,
      affinity: true,
      enabled: true,
    };
  }
}

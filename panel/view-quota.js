// view-quota.js — cached upstream quota windows, refreshed only on operator action (or opt-in load).
//
// Provider snapshots and observed gateway traffic are different things. This view only renders
// UsageQuota snapshots from GET /api/quota; it never turns the historical usage aggregate into a
// provider allowance.

import { api } from './api.js';
import { h, emptyState, errorBlock, icon, toggle } from './dom.js';
import {
  QUOTA_STATUS_CRITICAL_AT_OR_BELOW, QUOTA_STATUS_HEALTHY_ABOVE,
  formatQuotaAge, formatQuotaUsage, formatPercent, formatResetInLabel,
  mergeQuotaRecord, quotaAccountVisible, quotaFamilyLabel, quotaStatusLevel, quotaWindows,
  worstQuotaStatus,
} from './format.js';

// Bars by default: a share of a window is a share, and twenty cells made an operator count them.
// 'chunks' remains supported for a config that asked for it.
const DEFAULT_DISPLAY = Object.freeze({ mode: 'percentage', chunks: 20, reset_on_load: false });

// Display order is a local preference: it changes nothing about how quota is fetched or which
// account serves a request. Providers default to the config order; accounts default to the order
// the quota endpoint returned. A drag writes only the list it changed.
const QUOTA_PROVIDER_ORDER_KEY = 'manarouter.quota.provider-order';
const QUOTA_ACCOUNT_ORDER_KEY = 'manarouter.quota.account-order';

function readStoredList(key) {
  try {
    const value = JSON.parse(localStorage.getItem(key) ?? '[]');
    return Array.isArray(value) ? value.filter((id) => typeof id === 'string' && id) : [];
  } catch {
    return [];
  }
}

function readStoredOrderMap(key) {
  try {
    const value = JSON.parse(localStorage.getItem(key) ?? '{}');
    if (!value || typeof value !== 'object' || Array.isArray(value)) return {};
    const out = {};
    for (const [provider, ids] of Object.entries(value)) {
      if (Array.isArray(ids)) out[provider] = ids.filter((id) => typeof id === 'string' && id);
    }
    return out;
  } catch {
    return {};
  }
}

function storeOrder(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
    return true;
  } catch {
    return false;
  }
}

export async function renderQuota(mount, ctx) {
  let providers = [];
  let config = null;
  let display = { ...DEFAULT_DISPLAY };
  let quotaError = null;
  let configError = null;
  let usage = null;
  let usageError = null;
  let records = new Map();
  let refreshAllBusy = false;
  const busyAccounts = new Set();
  const configFieldAvailable = { value: false };
  const localErrors = new Map();
  let providerOrder = readStoredList(QUOTA_PROVIDER_ORDER_KEY);
  let accountOrder = readStoredOrderMap(QUOTA_ACCOUNT_ORDER_KEY);
  let dragAccount = null;
  let dragProviderId = null;

  mount.replaceChildren(h('div', { class: 'card' }, 'Loading provider quota…'));
  const [providerResult, configResult, quotaResult, usageResult] = await Promise.allSettled([
    ctx.loadProviders(),
    api.get('/api/config'),
    api.get('/api/quota'),
    api.get('/api/usage?limit=20'),
  ]);

  if (usageResult.status === 'fulfilled') usage = usageResult.value ?? null;
  else usageError = `Could not load request usage: ${usageResult.reason?.message || 'request failed'}`;

  if (providerResult.status === 'fulfilled') providers = providerResult.value ?? [];
  else quotaError = `Could not load accounts: ${providerResult.reason?.message || 'request failed'}`;

  if (configResult.status === 'fulfilled') {
    config = configResult.value?.config ?? configResult.value;
    const saved = config?.quota?.display;
    if (saved && Object.hasOwn(saved, 'reset_on_load')) {
      configFieldAvailable.value = true;
      display = {
        mode: saved.mode === 'percentage' ? 'percentage' : 'chunks',
        chunks: finitePositiveInteger(saved.chunks, DEFAULT_DISPLAY.chunks),
        reset_on_load: saved.reset_on_load === true,
      };
    }
  } else {
    configError = configResult.reason?.message || 'Could not read panel configuration.';
  }

  if (quotaResult.status === 'fulfilled') {
    records = recordsFrom(quotaResult.value);
  } else {
    quotaError = [quotaError, `Could not load cached provider quota: ${quotaResult.reason?.message || 'request failed'}`]
      .filter(Boolean).join(' ');
  }

  let accountRows = arrangeAccountRows(buildAccountRows(providers, records));
  render();

  // The default is off. Only the documented config field can opt into a provider request on view load.
  if (display.reset_on_load) await refreshAll();

  function render() {
    const toolbar = h('div', { class: 'toolbar' });
    const refreshButton = h('button', {
      class: 'btn btn-primary',
      type: 'button',
      disabled: refreshAllBusy || busyAccounts.size > 0,
      onclick: refreshAll,
    }, refreshAllBusy ? 'Refreshing…' : 'Refresh all');
    toolbar.append(refreshButton);

    if (configFieldAvailable.value) {
      const loadToggle = toggle('Refresh on entering Quota', {
        checked: display.reset_on_load,
        disabled: refreshAllBusy || busyAccounts.size > 0,
        hint: 'Off by default. When enabled, opening this page contacts each provider account.',
        onChange: (event) => saveRefreshOnLoad(event.currentTarget.checked),
      });
      toolbar.append(loadToggle.row);
    }

    const children = [toolbar];
    if (quotaError) children.push(errorBlock({ message: quotaError }));
    if (configError) children.push(errorBlock({ message: `Quota display settings unavailable: ${configError}` }));
    if (!configError && !configFieldAvailable.value) {
      children.push(h('p', { class: 'hint' },
        'Refresh-on-load is unavailable: the config API did not return quota.display.reset_on_load.'));
    }
    // The colour scale is stated once, in words, before any bar uses it. Colour repeats a number
    // the row already spells out; it never carries a fact on its own.
    children.push(h('p', { class: 'quota-legend' },
      h('span', { class: 'quota-legend-dot quota-level-ok', 'aria-hidden': 'true' }),
      ` above ${QUOTA_STATUS_HEALTHY_ABOVE}% left `,
      h('span', { class: 'quota-legend-dot quota-level-alert', 'aria-hidden': 'true' }),
      ` ${QUOTA_STATUS_HEALTHY_ABOVE}% or less `,
      h('span', { class: 'quota-legend-dot quota-level-critical', 'aria-hidden': 'true' }),
      ` ${QUOTA_STATUS_CRITICAL_AT_OR_BELOW}% or less`));

    // Allowances first, traffic second: the question this page answers is "how much is left",
    // and the request ledger is the supporting detail underneath it.
    if (!accountRows.length) {
      children.push(emptyState('No provider reports a quota.',
        'Add an account, or check a provider whose upstream exposes its remaining allowance.'));
    } else {
      // Accounts are grouped under their provider, and both levels can be reordered. Keeping one
      // flat grid made "which of these belong together" a question of reading every card title.
      const groups = groupAccountRows(accountRows);
      accountRows = groups.flatMap((group) => group.rows);
      children.push(renderGlance(groups));
      groups.forEach((group, index) => children.push(renderProviderGroup(group, index, groups.length)));
    }
    children.push(renderUsage());
    mount.replaceChildren(...children);
  }

  // The request ledger: what the client keys spent, with the token count reported by the provider
  // or estimated by the router. The estimate is computed in the panel process, so this page never
  // has to ask the relay for a number it does not have.
  function renderUsage() {
    const card = h('section', { class: 'card' });
    card.append(h('h2', {}, 'Recent request usage'));
    if (usageError) {
      card.append(errorBlock({ message: usageError }));
      return card;
    }
    const rows = Array.isArray(usage?.rows) ? usage.rows : [];
    if (!rows.length) {
      card.append(h('p', { class: 'hint' },
        'No request usage recorded yet. A request that carries a client key is recorded once it settles.'));
      return card;
    }
    const table = h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'When'),
        h('th', { scope: 'col' }, 'Key'),
        h('th', { scope: 'col' }, 'Prompt'),
        h('th', { scope: 'col' }, 'Completion'),
        h('th', { scope: 'col' }, 'Cached'),
        h('th', { scope: 'col' }, 'Cache write'),
        h('th', { scope: 'col' }, 'tokens/s'),
        h('th', { scope: 'col' }, 'Source'))));
    const body = h('tbody');
    for (const row of rows) {
      body.append(h('tr', {},
        h('td', { class: 'mono' }, formatUsageTime(row?.occurred_at)),
        h('td', { class: 'mono' }, row?.client_key_id ?? '(unknown)'),
        h('td', { class: 'num' }, formatTokens(row?.prompt_tokens)),
        h('td', { class: 'num' }, formatTokens(row?.completion_tokens)),
        // The cache split and the rate are shown as em dashes when the provider did not report
        // them. A zero would claim the provider said "no cache", which is a different fact from
        // silence, and the distinction is the whole reason these columns are nullable.
        h('td', { class: 'num' }, formatTokens(row?.cached_tokens)),
        h('td', { class: 'num' }, formatTokens(row?.cache_write_tokens)),
        h('td', { class: 'num' }, formatRate(row?.tokens_per_second)),
        h('td', {}, row?.estimated ? 'estimated by the router' : 'reported by the provider')));
    }
    table.append(body);
    card.append(table);
    const totals = usage?.totals ?? {};
    card.append(h('p', { class: 'hint' },
      `${totals.requests ?? rows.length} request(s), `
      + `${formatTokens(totals.prompt_tokens)} prompt tokens, `
      + `${formatTokens(totals.completion_tokens)} completion tokens`
      + (totals.cached_tokens ? `, ${formatTokens(totals.cached_tokens)} from cache` : '')
      + (totals.cache_write_tokens ? `, ${formatTokens(totals.cache_write_tokens)} written to cache` : '')
      + (totals.estimated_requests ? `, ${totals.estimated_requests} estimated` : '')));
    return card;
  }

  function accountLabel(row) {
    const record = records.get(row.key);
    const { account } = row;
    return account.label || account.identity || record?.label || account.id || record?.account || 'Account';
  }

  // One line per account, its windows as colour-coded chips. The cards below are the evidence;
  // this strip answers the question the page exists for — what is running out, right now. A chip
  // carries its own countdown, so "when does it come back" is answered without a second line, and
  // an account the provider never quantified has no line here at all.
  function renderGlance(groups) {
    const card = h('section', { class: 'card' });
    card.append(h('h2', {}, 'At a glance'));
    const list = h('div', { class: 'quota-glance' });
    let chips = 0;
    for (const group of groups) {
      const providerLine = h('div', { class: 'quota-glance-provider' },
        h('span', { class: 'mono' }, group.provider.id),
        group.provider.label && group.provider.label !== group.provider.id
          ? h('span', { class: 'hint' }, group.provider.label) : '');
      list.append(providerLine);
      for (const row of group.rows) {
        const windows = quotaWindows(records.get(row.key)?.windows)
          .map((window) => ({ window, usage: formatQuotaUsage(window, display) }))
          .filter(({ usage }) => usage.kind === 'percentage' || usage.kind === 'chunks');
        if (!windows.length) continue;
        const index = accountRows.indexOf(row);
        const label = accountLabel(row);
        const line = h('div', { class: 'quota-glance-account' },
          h('span', { class: 'quota-glance-name', title: `${row.provider.id} · ${label}` }, label));
        const rowChips = h('div', { class: 'quota-glance-chips' });
        for (const { window, usage } of windows) {
          const level = quotaStatusLevel(usage.remainingPercent);
          const family = quotaFamilyLabel(window?.family) || window?.display_name || 'window';
          const reset = formatResetInLabel(window?.reset_at);
          rowChips.append(h('button', {
            class: `quota-chip quota-level-${level}`,
            type: 'button',
            title: `${family}: ${formatPercent(usage.remainingPercent)} left · ${reset}`,
            onclick: () => scrollToAccount(index),
          },
          `${family} ${formatPercent(usage.remainingPercent)}`,
          h('span', { class: 'quota-chip-reset' }, reset)));
          chips += 1;
        }
        line.append(rowChips);
        list.append(line);
      }
    }
    if (!chips) {
      card.append(h('p', { class: 'hint' }, 'No cached quota windows yet. Refresh an account to fetch its allowance.'));
      return card;
    }
    card.append(list);
    card.append(h('p', { class: 'hint' },
      'Remaining share of each window, coloured by the scale above. Select a chip to jump to the account.'));
    return card;
  }

  function scrollToAccount(index) {
    document.getElementById(accountCardId(index))?.scrollIntoView({ block: 'start' });
  }

  /// One provider, its heading, and the account cards inside it. The whole block is a drag target;
  /// the heading's grip and arrows are the keyboard and discoverable equivalent.
  function renderProviderGroup(group, index, total) {
    const { provider, rows } = group;
    const head = h('div', {
      class: 'quota-provider-head',
      draggable: 'true',
      title: 'Drag to move this provider block',
      'data-quota-provider': provider.id,
    });
    const grip = h('span', { class: 'icon-btn quota-grip', 'aria-hidden': 'true' }, icon('grip'));
    const up = h('button', {
      class: 'icon-btn quota-provider-move', type: 'button',
      disabled: index === 0,
      'aria-label': 'Move ' + provider.id + ' block up', title: 'Move provider up',
    }, icon('up'));
    const down = h('button', {
      class: 'icon-btn quota-provider-move', type: 'button',
      disabled: index === total - 1,
      'aria-label': 'Move ' + provider.id + ' block down', title: 'Move provider down',
    }, icon('down'));
    up.addEventListener('click', (e) => { e.stopPropagation(); moveProviderBlock(provider.id, -1); });
    down.addEventListener('click', (e) => { e.stopPropagation(); moveProviderBlock(provider.id, 1); });
    head.append(grip, up, down,
      h('h2', {}, provider.id),
      provider.label && provider.label !== provider.id
        ? h('span', { class: 'quota-provider-label', title: provider.label }, provider.label) : '',
      h('span', { class: 'quota-provider-count' },
        `${rows.length} quota${rows.length === 1 ? '' : 's'}`));

    const cards = h('div', { class: 'quota-cards' });
    for (const row of rows) cards.append(renderAccount(row, group));
    const section = h('section', {
      class: 'quota-provider',
      'data-quota-provider': provider.id,
      'aria-label': `${provider.id} quota`,
    }, head, cards);

    head.addEventListener('dragstart', (e) => {
      if (e.target.closest('button, a, input, select, textarea')) { e.preventDefault(); return; }
      dragProviderId = provider.id;
      dragAccount = null;
      if (e.dataTransfer) {
        e.dataTransfer.effectAllowed = 'move';
        try { e.dataTransfer.setData('text/plain', provider.id); } catch { /* Firefox only for external drags. */ }
      }
      section.classList.add('dragging');
    });
    head.addEventListener('dragend', clearDragMarks);
    section.addEventListener('dragover', (e) => {
      if (!dragProviderId || dragProviderId === provider.id) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
      section.classList.add('drag-over');
    });
    section.addEventListener('dragleave', (e) => {
      if (!section.contains(e.relatedTarget)) section.classList.remove('drag-over');
    });
    section.addEventListener('drop', (e) => {
      if (!dragProviderId || dragProviderId === provider.id) return;
      e.preventDefault();
      e.stopPropagation();
      const source = dragProviderId;
      clearDragMarks();
      reorderProviderBlock(source, provider.id);
    });
    return section;
  }

  function clearDragMarks() {
    dragAccount = null;
    dragProviderId = null;
    mount.querySelectorAll('.dragging, .drag-over')
      .forEach((node) => node.classList.remove('dragging', 'drag-over'));
  }

  function renderAccount(row, group) {
    const { provider, key } = row;
    const index = accountRows.indexOf(row);
    const position = group.rows.indexOf(row);
    const record = records.get(key);
    const label = accountLabel(row);
    // Only windows the provider quantified are drawn — one row if it reports one. An account that
    // quantified nothing never reaches this function: buildAccountRows dropped it.
    const windows = quotaWindows(record?.windows);
    // A single exhausted window decides the account's tone; averaging the windows would bury it.
    const worst = worstQuotaStatus(windows.map((window) => {
      const usage = formatQuotaUsage(window, display);
      return usage.kind === 'percentage' || usage.kind === 'chunks'
        ? quotaStatusLevel(usage.remainingPercent) : 'unknown';
    }));
    const card = h('section', {
      class: `card quota-account quota-level-${worst}`,
      id: accountCardId(index),
      draggable: 'true',
      'data-quota-account': row.account.id ?? '',
      'aria-label': `${provider.id} · ${label}`,
    });
    const grip = h('span', {
      class: 'icon-btn quota-grip', 'aria-hidden': 'true',
      title: 'Drag to reorder this quota inside ' + provider.id,
    }, icon('grip'));
    const up = h('button', {
      class: 'icon-btn quota-account-move', type: 'button',
      disabled: position === 0,
      'aria-label': 'Move ' + label + ' up', title: 'Move quota up',
    }, icon('up'));
    const down = h('button', {
      class: 'icon-btn quota-account-move', type: 'button',
      disabled: position === group.rows.length - 1,
      'aria-label': 'Move ' + label + ' down', title: 'Move quota down',
    }, icon('down'));
    up.addEventListener('click', (e) => { e.stopPropagation(); moveAccountRow(row, -1); });
    down.addEventListener('click', (e) => { e.stopPropagation(); moveAccountRow(row, 1); });
    const titleWrap = h('div', { class: 'quota-account-title' }, grip, up, down, h('h2', { title: `${provider.id} · ${label}` }, label));
    const header = h('div', { class: 'toolbar quota-account-head' }, titleWrap);
    const refreshButton = h('button', {
      class: 'btn',
      type: 'button',
      disabled: refreshAllBusy || busyAccounts.has(key),
      onclick: () => refreshOne(row),
    }, busyAccounts.has(key) ? 'Refreshing…' : 'Refresh');
    header.append(refreshButton);
    card.append(header);

    card.addEventListener('dragstart', (e) => {
      if (e.target.closest('button, a, input, select, textarea')) { e.preventDefault(); return; }
      e.stopPropagation();
      dragAccount = row;
      dragProviderId = null;
      if (e.dataTransfer) {
        e.dataTransfer.effectAllowed = 'move';
        try { e.dataTransfer.setData('text/plain', `${provider.id}/${row.account.id}`); } catch { /* Firefox */ }
      }
      card.classList.add('dragging');
    });
    card.addEventListener('dragend', clearDragMarks);
    card.addEventListener('dragover', (e) => {
      if (!dragAccount || dragAccount.provider.id !== provider.id || dragAccount.key === key) return;
      e.preventDefault();
      e.stopPropagation();
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
      card.classList.add('drag-over');
    });
    card.addEventListener('dragleave', (e) => {
      if (!card.contains(e.relatedTarget)) card.classList.remove('drag-over');
    });
    card.addEventListener('drop', (e) => {
      if (!dragAccount || dragAccount.provider.id !== provider.id || dragAccount.key === key) return;
      e.preventDefault();
      e.stopPropagation();
      const source = dragAccount;
      clearDragMarks();
      reorderAccountRow(source, row);
    });

    // Age only. The snapshot's source URL and its per-model `details` remain in the API for
    // diagnostics; printed on every card they buried the number this page exists to show.
    card.append(h('p', { class: 'hint' }, formatQuotaAge(record?.fetched_at)));

    const error = localErrors.get(key) || record?.error;
    if (error) card.append(errorBlock({ message: errorMessage(error) }));

    for (const window of windows) {
      card.append(renderWindow(quotaFamilyLabel(window?.family) || window?.display_name || 'Window', window));
    }
    return card;
  }

  // One window: name · bar · readout, with the reset countdown on the right. The bar fills with
  // what is REMAINING — that is the number an operator acts on — and its colour repeats the
  // percentage written beside it instead of replacing it.
  //
  // Only quantified windows arrive here (see quotaWindows), so there is no N/A branch: a row that
  // exists always has a number to show.
  function renderWindow(label, window) {
    const usage = formatQuotaUsage(window, display);
    const level = usage.kind === 'percentage' || usage.kind === 'chunks'
      ? quotaStatusLevel(usage.remainingPercent) : 'unknown';
    const row = h('div', { class: `quota-window quota-level-${level}` });
    row.append(h('strong', { class: 'quota-window-name' }, label));
    const info = h('div', { class: 'quota-window-usage' });
    if (usage.kind === 'unlimited') {
      info.append(h('span', { class: 'chunks-na' }, 'Unlimited'));
    } else if (usage.kind === 'percentage') {
      info.append(percentageBar(usage), h('span', { class: 'quota-readout' },
        h('strong', { class: 'quota-pct' }, `${formatPercent(usage.remainingPercent)} left`),
        h('span', { class: 'quota-used' }, `${formatPercent(usage.usedPercent)} used`)));
    } else {
      const cells = h('div', { class: 'chunks', role: 'img',
        'aria-label': `${formatPercent(usage.remainingPercent)} left, ${formatPercent(usage.usedPercent)} used` });
      for (let index = 0; index < usage.total; index += 1) {
        const remainingCells = usage.remaining;
        const isHalf = usage.half && index === remainingCells;
        cells.append(h('span', { class: `chunk${index < remainingCells ? ' on' : isHalf ? ' half' : ''}` }));
      }
      info.append(cells, h('span', { class: 'quota-readout' },
        h('strong', { class: 'quota-pct' }, `${formatPercent(usage.remainingPercent)} left`),
        h('span', { class: 'quota-used' }, `${formatPercent(usage.usedPercent)} used`)));
    }
    info.append(h('span', { class: 'quota-reset-in' }, formatResetInLabel(window?.reset_at)));
    row.append(info);
    return row;
  }

  function percentageBar(usage) {
    const bar = h('div', { class: 'quota-bar', role: 'img',
      'aria-label': `${formatPercent(usage.remainingPercent)} left, ${formatPercent(usage.usedPercent)} used` });
    const fill = h('span', { class: 'quota-bar-fill', 'aria-hidden': 'true' });
    fill.style.width = `${Math.min(100, Math.max(0, usage.remainingPercent))}%`;
    bar.append(fill);
    return bar;
  }

  async function refreshOne(row) {
    const { provider, account, key } = row;
    if (refreshAllBusy || busyAccounts.has(key)) return;
    busyAccounts.add(key);
    localErrors.delete(key);
    render();
    try {
      const data = await api.post(`/api/providers/${encodeURIComponent(provider.id)}/accounts/${encodeURIComponent(account.id)}/quota/refresh`);
      mergeRecords(recordsFrom(data), key);
      accountRows = buildAccountRows(providers, records);
      if (!records.get(key)?.error) localErrors.delete(key);
    } catch (err) {
      localErrors.set(key, err?.message || 'Quota refresh failed.');
    } finally {
      busyAccounts.delete(key);
      render();
    }
  }

  async function refreshAll() {
    if (refreshAllBusy || busyAccounts.size > 0) return;
    refreshAllBusy = true;
    for (const key of records.keys()) localErrors.delete(key);
    render();
    try {
      const data = await api.post('/api/quota/refresh');
      mergeRecords(recordsFrom(data));
      accountRows = buildAccountRows(providers, records);
      quotaError = null;
    } catch (err) {
      quotaError = `Refresh all failed: ${err?.message || 'request failed'}`;
    } finally {
      refreshAllBusy = false;
      render();
    }
  }

  function mergeRecords(incoming, onlyKey = null) {
    for (const [key, next] of incoming) {
      if (onlyKey && key !== onlyKey) continue;
      // A per-account error must not erase its last good cached snapshot.
      records.set(key, mergeQuotaRecord(records.get(key), next));
      if (next.error) localErrors.set(key, errorMessage(next.error));
      else localErrors.delete(key);
    }
  }

  // ---------------------------------------------------------------- order and grouping

  /// Group the visible rows by provider, applying the local display order at both levels.
  /// A provider or account the stored order has never seen keeps its server/config position
  /// instead of jumping to the bottom of an otherwise untouched page.
  function groupAccountRows(rows) {
    const groups = new Map();
    for (const row of rows) {
      const id = row.provider.id;
      if (!groups.has(id)) groups.set(id, { provider: row.provider, rows: [] });
      groups.get(id).rows.push(row);
    }

    const configRank = new Map(providers.map((provider, index) => [provider.id, index]));
    const storedProviderRank = new Map(providerOrder.map((id, index) => [id, index]));
    const ordered = [...groups.values()].sort((a, b) => {
      const ar = storedProviderRank.get(a.provider.id);
      const br = storedProviderRank.get(b.provider.id);
      if (ar !== undefined || br !== undefined) {
        if (ar === undefined) return 1;
        if (br === undefined) return -1;
        return ar - br;
      }
      return (configRank.get(a.provider.id) ?? Number.MAX_SAFE_INTEGER)
        - (configRank.get(b.provider.id) ?? Number.MAX_SAFE_INTEGER);
    });

    for (const group of ordered) {
      const originalRank = new Map(group.rows.map((row, index) => [row.key, index]));
      const storedAccountRank = new Map((accountOrder[group.provider.id] ?? [])
        .map((id, index) => [id, index]));
      group.rows.sort((a, b) => {
        const ar = storedAccountRank.get(a.account.id);
        const br = storedAccountRank.get(b.account.id);
        if (ar !== undefined || br !== undefined) {
          if (ar === undefined) return 1;
          if (br === undefined) return -1;
          return ar - br;
        }
        return (originalRank.get(a.key) ?? 0) - (originalRank.get(b.key) ?? 0);
      });
    }
    return ordered;
  }

  function arrangeAccountRows(rows) {
    return groupAccountRows(rows).flatMap((group) => group.rows);
  }

  function saveProviderOrder(groups) {
    providerOrder = groups.map((group) => group.provider.id);
    if (!storeOrder(QUOTA_PROVIDER_ORDER_KEY, providerOrder)) {
      ctx.toast.warning('This browser could not save the quota provider order.');
    }
  }

  function saveAccountOrder(providerId, rows) {
    accountOrder = { ...accountOrder, [providerId]: rows.map((row) => row.account.id) };
    if (!storeOrder(QUOTA_ACCOUNT_ORDER_KEY, accountOrder)) {
      ctx.toast.warning('This browser could not save the quota order inside ' + providerId + '.');
    }
  }

  function commitProviderGroups(groups) {
    saveProviderOrder(groups);
    accountRows = groups.flatMap((group) => group.rows);
    render();
  }

  function commitAccountGroup(groups, providerId) {
    const group = groups.find((candidate) => candidate.provider.id === providerId);
    if (group) saveAccountOrder(providerId, group.rows);
    accountRows = groups.flatMap((candidate) => candidate.rows);
    render();
  }

  function focusProviderBlock(id) {
    mount.querySelector(`[data-quota-provider="${CSS.escape(id)}"] .quota-provider-move:not(:disabled)`)?.focus();
  }

  function focusAccountRow(row) {
    const index = accountRows.findIndex((candidate) => candidate.key === row.key);
    if (index >= 0) {
      mount.querySelector(`#${accountCardId(index)} .quota-account-move:not(:disabled)`)?.focus();
    }
  }

  function moveProviderBlock(id, delta) {
    const groups = groupAccountRows(accountRows);
    const from = groups.findIndex((group) => group.provider.id === id);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= groups.length) return;
    [groups[from], groups[to]] = [groups[to], groups[from]];
    commitProviderGroups(groups);
    focusProviderBlock(id);
  }

  function reorderProviderBlock(sourceId, targetId) {
    if (sourceId === targetId) return;
    const groups = groupAccountRows(accountRows);
    const from = groups.findIndex((group) => group.provider.id === sourceId);
    if (from < 0) return;
    const [moved] = groups.splice(from, 1);
    const insertAt = groups.findIndex((group) => group.provider.id === targetId);
    groups.splice(insertAt < 0 ? groups.length : insertAt, 0, moved);
    commitProviderGroups(groups);
  }

  function moveAccountRow(row, delta) {
    const groups = groupAccountRows(accountRows);
    const group = groups.find((candidate) => candidate.provider.id === row.provider.id);
    if (!group) return;
    const from = group.rows.findIndex((candidate) => candidate.key === row.key);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= group.rows.length) return;
    [group.rows[from], group.rows[to]] = [group.rows[to], group.rows[from]];
    commitAccountGroup(groups, row.provider.id);
    focusAccountRow(row);
  }

  function reorderAccountRow(source, target) {
    if (source.provider.id !== target.provider.id || source.key === target.key) return;
    const groups = groupAccountRows(accountRows);
    const group = groups.find((candidate) => candidate.provider.id === source.provider.id);
    if (!group) return;
    const from = group.rows.findIndex((candidate) => candidate.key === source.key);
    if (from < 0) return;
    const [moved] = group.rows.splice(from, 1);
    const insertAt = group.rows.findIndex((candidate) => candidate.key === target.key);
    group.rows.splice(insertAt < 0 ? group.rows.length : insertAt, 0, moved);
    commitAccountGroup(groups, source.provider.id);
  }

  async function saveRefreshOnLoad(enabled) {
    if (!config || !configFieldAvailable.value) return;
    const previous = display.reset_on_load;
    display.reset_on_load = Boolean(enabled);
    render();
    try {
      const next = JSON.parse(JSON.stringify(config));
      next.quota ??= {};
      next.quota.display ??= {};
      next.quota.display.reset_on_load = display.reset_on_load;
      const saved = await api.put('/api/config', next);
      config = saved?.config ?? saved ?? next;
      display.reset_on_load = config?.quota?.display?.reset_on_load === true;
      configError = null;
    } catch (err) {
      display.reset_on_load = previous;
      configError = `Could not save refresh-on-load setting: ${err?.message || 'request failed'}`;
    }
    render();
  }
}

/** A stable DOM id for one account card, so a chip can jump to it. Index-based: keys are opaque. */
function accountCardId(index) {
  return `quota-account-${index}`;
}

// A token count that is not known is not zero: it is shown as a dash.
function formatTokens(value) {
  return value == null ? '—' : String(value);
}

// A rate is reported to one decimal place; `null` means the provider gave no timing, so there is
// no rate to state and the cell says so rather than showing a zero.
function formatRate(value) {
  if (value == null || !Number.isFinite(Number(value))) return '—';
  return Number(value).toFixed(1);
}

// The ledger stores milliseconds since the epoch; the panel shows it in the local timezone.
function formatUsageTime(millis) {
  if (millis == null) return '—';
  const date = new Date(Number(millis));
  if (Number.isNaN(date.getTime())) return String(millis);
  return date.toLocaleString();
}

function recordsFrom(data) {
  const entries = Array.isArray(data) ? data
    : Array.isArray(data?.accounts) ? data.accounts
      : Array.isArray(data?.results) ? data.results
        : data?.account != null && (Array.isArray(data?.windows) || data?.error) ? [data]
          : [];
  const result = new Map();
  for (const entry of entries) {
    if (!entry || typeof entry !== 'object') continue;
    // The pre-F1 GET /api/quota returned historical gateway-usage totals. Those are not provider
    // quota snapshots, so do not surface them as if they were cached allowance data.
    if (!Array.isArray(entry.windows) && entry.error == null && !Object.hasOwn(entry, 'fetched_at')) continue;
    const provider = typeof entry.provider === 'string' ? entry.provider : entry.provider?.id;
    const account = typeof entry.account === 'string' ? entry.account : entry.account?.id;
    if (!provider || !account) continue;
    result.set(recordKey(provider, account), { ...entry, provider, account });
  }
  return result;
}

function buildAccountRows(providers, records) {
  // The control plane decides which accounts have a quota worth showing: it omits the ones whose
  // provider reports no allowance at all. Rendering every configured account here would put those
  // back on the page with invented `0/0` windows, so the rows come from the quota response.
  //
  // Even inside that response, an account is only a row when it has a quantified window to draw.
  // An error alone is not a quota card: without a window the card would say "Never fetched" and
  // repeat the log. After a failed refresh, mergeRecords keeps the last-good window, so the error
  // is still displayed on the card it belongs to.
  const rows = [];
  const byProvider = new Map(providers.map((provider) => [provider.id, provider]));
  for (const [key, record] of records) {
    if (!quotaAccountVisible(record)) continue;
    const provider = byProvider.get(record.provider) ?? { id: record.provider || '(unknown provider)' };
    const account = (provider.accounts ?? []).find((candidate) => candidate.id === record.account)
      ?? { id: record.account, label: record.label || record.account };
    rows.push({ provider, account, key });
  }
  return rows;
}

function recordKey(provider, account) {
  return `${provider}\u0000${account}`;
}

function errorMessage(error) {
  if (typeof error === 'string') return error;
  return error?.message || error?.error?.message || 'Quota refresh failed for this account.';
}

function finitePositiveInteger(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? Math.min(200, Math.floor(number)) : fallback;
}

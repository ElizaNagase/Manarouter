// search-select.js — a text input with an in-page suggestion list.
//
// A native <datalist> is drawn by the browser, not by the page: its ordering differs between
// browsers, it can wait for a pause before filtering, and its rows cannot carry a hint or a
// distinct "this is a combo" marker. This is the same idea owned by the panel, so the list
// filters on every keystroke, is alphabetical by default, and works from the keyboard.

import { h } from './dom.js';

/** Alphabetical by the visible label, then by value; never mutates the input. */
export function sortOptions(options) {
  return [...(options ?? [])].sort((a, b) => {
    const byLabel = String(a?.label ?? a?.value ?? '').localeCompare(
      String(b?.label ?? b?.value ?? ''),
      undefined,
      { sensitivity: 'base', numeric: true },
    );
    if (byLabel !== 0) return byLabel;
    return String(a?.value ?? '').localeCompare(String(b?.value ?? ''), undefined, { numeric: true });
  });
}

/**
 * The suggestions for a query. Filtering is a case-insensitive substring over the value, label
 * and hint; the result keeps the alphabetical order, because a list that reorders while you type
 * makes the next click a guess.
 */
export function filterOptions(options, query, { limit = 200 } = {}) {
  const sorted = sortOptions(options);
  const needle = String(query ?? '').trim().toLowerCase();
  const matched = needle
    ? sorted.filter((option) => [option?.value, option?.label, option?.hint]
      .join(' ').toLowerCase().includes(needle))
    : sorted;
  return matched.slice(0, limit);
}

let uid = 0;

/**
 * Build the combobox. Returns:
 *   el       the wrapper to place in the page
 *   input    the text input (focus/validation targets it)
 *   value()  the current text, trimmed
 *   setOptions(next)  replace the suggestions (the selected text is left alone)
 *   setValue(text)    set the text without firing onChoose
 *   close()
 *
 * onChoose(option) fires for a click or Enter on a suggestion, but typing remains allowed:
 * the model field is also how an operator names a model that the provider list does not contain.
 */
export function createSearchSelect({
  ariaLabel,
  placeholder = '',
  value = '',
  emptyText = 'no matches',
  onChoose,
  onInput,
} = {}) {
  uid += 1;
  const listId = 'search-select-' + uid;
  let options = [];
  let matches = [];
  let active = -1;
  let expanded = false;

  const input = h('input', {
    class: 'input',
    type: 'text',
    role: 'combobox',
    autocomplete: 'off',
    spellcheck: 'false',
    'aria-label': ariaLabel,
    'aria-autocomplete': 'list',
    'aria-controls': listId,
    'aria-expanded': 'false',
    placeholder,
    value,
  });
  const list = h('ul', { class: 'search-select-list', id: listId, role: 'listbox', hidden: true });
  const wrap = h('div', { class: 'search-select' }, input, list);

  function paint() {
    matches = filterOptions(options, input.value);
    list.replaceChildren();
    if (!matches.length) {
      list.append(h('li', { class: 'search-select-empty', role: 'presentation' }, emptyText));
    } else {
      matches.forEach((option, i) => {
        const row = h('li', {
          class: i === active ? 'active' : null,
          role: 'option',
          id: listId + '-opt-' + i,
          'aria-selected': i === active ? 'true' : 'false',
          title: option.title ?? null,
          // mousedown would move focus out of the input before click lands; keeping focus makes
          // the click behave exactly like Enter.
          onmousedown: (e) => e.preventDefault(),
          onclick: () => choose(option),
        },
        h('span', {}, option.label ?? option.value),
        option.hint ? h('span', { class: 'search-select-hint' }, option.hint) : null);
        list.append(row);
      });
    }
    input.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    input.setAttribute('aria-activedescendant', active >= 0 && matches[active] ? listId + '-opt-' + active : '');
  }

  function open() {
    if (expanded) return;
    expanded = true;
    active = -1;
    list.hidden = false;
    paint();
  }

  function close() {
    expanded = false;
    active = -1;
    list.hidden = true;
    input.setAttribute('aria-expanded', 'false');
    input.removeAttribute('aria-activedescendant');
  }

  function choose(option) {
    input.value = option.value;
    close();
    onChoose?.(option);
  }

  function move(delta) {
    if (!expanded) open();
    if (!matches.length) return;
    active = (active + delta + matches.length) % matches.length;
    paint();
    list.children[active]?.scrollIntoView?.({ block: 'nearest' });
  }

  input.addEventListener('focus', () => open());
  input.addEventListener('click', () => open());
  input.addEventListener('input', () => {
    active = -1;
    open();
    paint();
    onInput?.(input.value);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); move(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
    else if (e.key === 'Enter' && expanded && active >= 0 && matches[active]) {
      e.preventDefault();
      choose(matches[active]);
    } else if (e.key === 'Escape' && expanded) {
      e.preventDefault();
      close();
    }
  });
  wrap.addEventListener('focusout', (e) => {
    if (!wrap.contains(e.relatedTarget)) close();
  });

  return {
    el: wrap,
    input,
    value: () => input.value.trim(),
    setOptions(next) {
      options = Array.isArray(next) ? next : [];
      if (expanded) paint();
    },
    setValue(text) {
      input.value = text ?? '';
    },
    close,
  };
}

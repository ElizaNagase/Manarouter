// view-docs.js — documentation browser (GET /api/docs → list, GET /api/docs/{name} → raw markdown).
//
// Documents are markdown and are rendered by markdown.js, which HTML-escapes the whole input
// before applying any formatting: raw HTML in a document shows up as visible text, never as
// markup. The RAW text is kept alongside the render so Download saves the source, not the HTML.
//
// SECURITY NOTE — the one innerHTML in the panel: `body.innerHTML = renderMarkdown(text)` below
// is safe for exactly one reason: renderMarkdown emits only its own fixed set of tags and has
// already escaped every character that came from the document. Do not assign anything else here.

import { api } from './api.js';
import { h, emptyState, errorBlock } from './dom.js';
import { extractList } from './format.js';
import { renderMarkdown } from './markdown.js';

export async function renderDocs(mount, ctx, selected) {
  const listEl = h('ul', { class: 'docs-nav' });
  const content = h('div', { class: 'card' });
  mount.replaceChildren(h('div', { class: 'docs-view' },
    h('div', { class: 'docs-layout' },
      h('div', { class: 'card docs-index' }, h('h2', {}, 'Documents'), listEl),
      content)));

  let docs = [];
  let loadSeq = 0; // last-request-wins: a slow older response must not overwrite a newer one
  try {
    docs = extractList(await api.get('/api/docs'), 'docs');
  } catch (err) {
    content.replaceChildren(errorBlock(err));
    return;
  }

  if (!docs.length) {
    listEl.append(h('li', { class: 'hint' }, 'none'));
    content.replaceChildren(emptyState('No documentation is available in this build.',
      'GET /api/docs returned an empty list.'));
    return;
  }

  // Selection: URL param, else the first document — the pane is never silently empty.
  const wanted = selected && docs.find((d) => docName(d) === selected) ? selected : docName(docs[0]);
  if (wanted !== selected) history.replaceState(null, '', `#/docs/${encodeURIComponent(wanted)}`);
  // After `wanted`: renderList() reads it to mark aria-current, and calling it before the
  // const would throw a ReferenceError (temporal dead zone) and kill the whole docs view.
  renderList();
  await load(wanted);

  function docName(d) {
    return d.name ?? d.path ?? d.title ?? '';
  }

  function renderList() {
    listEl.replaceChildren();
    for (const d of docs) {
      const btn = h('button', { class: 'docs-nav-item', type: 'button' },
        h('div', { class: 'dtitle' }, d.title ?? docName(d)),
        d.purpose ? h('div', { class: 'dpurpose' }, d.purpose) : null);
      btn.addEventListener('click', () => {
        history.replaceState(null, '', `#/docs/${encodeURIComponent(docName(d))}`);
        [...listEl.querySelectorAll('button')].forEach((b) => b.removeAttribute('aria-current'));
        btn.setAttribute('aria-current', 'true');
        load(docName(d));
      });
      if (docName(d) === wanted) btn.setAttribute('aria-current', 'true');
      listEl.append(h('li', {}, btn));
    }
  }

  async function load(name) {
    const seq = ++loadSeq;
    content.replaceChildren(h('p', { class: 'hint' }, 'loading…'));
    let text;
    try {
      text = await api.getRaw(`/api/docs/${encodeURIComponent(name)}`);
    } catch (err) {
      if (seq !== loadSeq) return;
      content.replaceChildren(errorBlock(err));
      return;
    }
    if (seq !== loadSeq) return;

    // Body of rendered markdown. The scroll and type scale live in lab-docs.css; this file only
    // decides what goes in the pane.
    const body = h('div', { class: 'md-body' });
    body.innerHTML = renderMarkdown(text); // safe — see the SECURITY NOTE at the top of this file

    // Most documents open with their own H1, which repeats the title already stated by the card
    // header above the body. Drop that one duplicate so the pane does not say the same thing twice;
    // a document whose first heading is something else keeps it.
    const docTitle = docs.find((d) => docName(d) === name)?.title ?? name;
    const firstHeading = body.querySelector('h1');
    if (firstHeading && firstHeading.textContent.trim() === String(docTitle).trim()) firstHeading.remove();

    const downloadBtn = h('button', { class: 'btn btn-ghost btn-sm', type: 'button' }, 'Download');
    downloadBtn.addEventListener('click', () => downloadRaw(text, name));

    // Header row: the document title, then the download action pushed to the far right by .spacer.
    content.replaceChildren(
      h('div', { class: 'docs-pane-head' },
        h('h2', { class: 'docs-doc-title' }, docTitle),
        h('span', { class: 'spacer' }),
        downloadBtn),
      body,
    );
  }
}

/** Save the RAW markdown (never the rendered HTML) as `<name>.md` via a Blob object URL. */
function downloadRaw(rawText, name) {
  const blob = new Blob([rawText], { type: 'text/markdown;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const link = h('a', { href: url, download: fileName(name) });
  document.body.append(link);
  link.click();
  link.remove();
  // Revoke on a later task: click() can return before a browser has finished reading the blob.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** `/api/docs` names are file stems, but never write `x.md.md` if one ever arrives with a suffix. */
function fileName(name) {
  return `${String(name).replace(/\.md$/i, '')}.md`;
}

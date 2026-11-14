'use strict';
// 分页与扫描只操作本地示例，不向数据库发送请求。
const resultPreviewLabels = {
  'result-single-pass': { zh: '单次读取结果', en: 'Single-pass result' },
  'result-memory-limit': { zh: '结果达到内存上限', en: 'Result at memory limit' },
  'result-expired': { zh: '结果已过期', en: 'Result expired' },
};
const resultPageSizes = [20, 50, 100, 200, 500, 1000];
// 扩充示例行，以标准页大小展示翻页，不改变真实数据库。
for (let n = 0; n < 105; n++) {
  const id = String(11000 + n);
  orders.push({
    id,
    order_no: `SAMPLE-${id}`,
    customer_id: '1',
    status: 'paid',
    total: '10.00',
    created_at: '2026-10-28 10:00:00',
  });
}
const resultPages = new Map();
function resultPageState(id, size = 100) {
  if (!resultPages.has(id)) resultPages.set(id, { page: 0, size, counted: false });
  return resultPages.get(id);
}
function resultMode() {
  return document.querySelector('#preview-state')?.value || 'default';
}
function resultPageSlice(items, id, size = 100) {
  if (resultMode() === 'result-expired') return [];
  const page = resultPageState(id, size);
  page.page = Math.min(page.page, Math.max(0, Math.ceil(items.length / page.size) - 1));
  return items.slice(page.page * page.size, (page.page + 1) * page.size);
}
function resultPageMarkup(markup, id, count, size = 100, matching = null) {
  if (resultMode() === 'result-expired') {
    return `<div class="empty small">${
      az('结果已过期，无法继续读取。', 'Result expired; further reading is unavailable.')
    }</div>`;
  }
  const page = resultPageState(id, size);
  page.page = Math.min(page.page, Math.max(0, Math.ceil(count / page.size) - 1));
  const template = document.createElement('template');
  template.innerHTML = markup;
  template.content.querySelectorAll('tbody > tr').forEach((row, index) => {
    if (
      index < page.page * page.size || index >= (page.page + 1) * page.size
      || (matching && !matching.has(row.dataset.row))
    ) row.remove();
  });
  return template.innerHTML;
}
function resultPager(id, count, options = {}) {
  const page = resultPageState(id, options.size || 100);
  const selectedMode = options.live ? resultMode() : 'default';
  const mode = options.singlePass && !selectedMode.startsWith('result-') ? 'result-single-pass' : selectedMode;
  const single = ['result-single-pass', 'result-memory-limit', 'result-expired'].includes(mode);
  const expired = mode === 'result-expired';
  const visible = expired ? 0 : options.visible ?? Math.max(0, Math.min(page.size, count - page.page * page.size));
  const countLabel = expired ? az('结果不可用', 'Result unavailable') : page.counted && options.live
    ? az(
      `示例统计：${options.total ?? count} 行（未发送请求）`,
      `Example count: ${options.total ?? count} rows (no request sent)`,
    )
    : options.live && !single
    ? az('总数未知', 'Total unknown')
    : az(`已保留 ${count} 行`, `${count} retained rows`);
  const status = expired
    ? az('结果已过期；不会自动重新执行。', 'Result expired; it will not be re-executed automatically.')
    : mode === 'result-memory-limit'
    ? az(
      '已达到内存上限，只能回看已保留页，不能继续读取。',
      'Memory limit reached; only retained pages can be reviewed, and reading cannot continue.',
    )
    : single
    ? az(
      '单次读取：只能回看已保留页，不会自动重新执行。',
      'Single pass: only retained pages can be reviewed; the query will not be re-executed automatically.',
    )
    : options.live
    ? az('本地示例；实际翻页会重新查询。', 'Local example; actual page changes re-query the source.')
    : '';
  const retained = single && !expired
    ? az(
      `保留范围：第 1–${Math.max(1, Math.ceil(count / page.size))} 页`,
      `Retained pages: 1–${Math.max(1, Math.ceil(count / page.size))}`,
    )
    : '';
  const button = (action, label, glyph, disabled) =>
    `<button type="button" class="icon-button result-page-button" data-result-page="${action}" data-result-id="${
      h(id)
    }" title="${h(label)}" aria-label="${h(label)}" ${disabled ? 'disabled' : ''}>${glyph}</button>`;
  const calculator =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5" y="3" width="14" height="18" rx="2"/><path d="M8 7h8v3H8zM8 14h.01M12 14h.01M16 14h.01M8 18h.01M12 18h.01M16 18h.01"/></svg>';
  return `<div class="table-footer result-pager"><span>${az('本页', 'Page')}: ${visible} · ${h(countLabel)}${
    status ? ` · ${retained ? `${h(retained)} · ` : ''}${h(status)}` : ''
  }</span><span class="spacer"></span><label class="page-size-control">${
    az('每页', 'Per page')
  } <select data-result-size="${h(id)}" aria-label="${az('结果每页行数', 'Result page size')}">${
    resultPageSizes.map(n => `<option value="${n}" ${n === page.size ? 'selected' : ''}>${n}</option>`).join('')
  }</select></label>${
    options.live && !single
      ? button('count', az('统计总行数（示例）', 'Count total rows (example)'), calculator, false)
      : ''
  }${button('previous', t('previousPage'), icon('back'), expired || !page.page)}<span class="mono">${
    page.counted && options.live
      ? az(
        `第 ${page.page + 1} / ${Math.max(1, Math.ceil((options.total ?? count) / page.size))} 页`,
        `Page ${page.page + 1} / ${Math.max(1, Math.ceil((options.total ?? count) / page.size))}`,
      )
      : az(`第 ${page.page + 1} 页`, `Page ${page.page + 1}`)
  }</span>${button('next', t('nextPage'), icon('chev-r'), expired || (page.page + 1) * page.size >= count)}</div>`;
}
const redisScanPreview = { signature: '', generation: 0, cursor: 0, pending: [], loaded: [], scanning: false };
function redisScanKeys() {
  const signature = JSON.stringify([state.redisDb, state.keyFilter, state.keyType]);
  if (signature !== redisScanPreview.signature) {
    redisScanPreview.signature = signature;
    redisScanPreview.generation++;
    redisScanPreview.loaded = [];
    redisScanPreview.scanning = false;
    redisScanPreview.pending = state.redisDb === 0
      ? keySamples.filter(key =>
        key.name.toLowerCase().includes(state.keyFilter.toLowerCase().replaceAll('*', ''))
        && (state.keyType === 'all' || key.type === state.keyType)
      )
      : [];
    state.selectedKeys.clear();
    state.keyAnchor = null;
    redisScanAdvance();
  }
  return redisScanPreview.loaded;
}
function redisScanAdvance() {
  const known = new Set(redisScanPreview.loaded.map(key => key.name));
  for (const key of redisScanPreview.pending.splice(0, 3)) {
    if (!known.has(key.name)) {
      redisScanPreview.loaded.push(key);
      known.add(key.name);
    }
  }
  redisScanPreview.cursor = redisScanPreview.pending.length ? 1 : 0;
}
function redisScanFooter() {
  const scan = redisScanPreview;
  const complete = !scan.cursor && !scan.pending.length;
  const label = scan.scanning
    ? az('加载中…', 'Loading…')
    : complete
    ? az('扫描完成', 'Scan complete')
    : az('可加载更多', 'More keys available');
  return `<div class="table-footer small redis-scan-footer"><span>${
    h(label)
  }</span><span class="spacer"></span><button type="button" class="button" data-result-scan ${
    scan.scanning || complete ? 'disabled' : ''
  }>${h(az('加载更多', 'Load more'))}</button></div>`;
}
function clearResultSelection() {
  state.selected.clear();
  state.anchor = null;
  state.selectedEntries.clear();
  state.entryAnchor = null;
  state.cellSelection = null;
}
document.addEventListener('click', event => {
  const scanButton = event.target.closest('[data-result-scan]');
  if (scanButton) {
    if (scanButton.disabled) return;
    event.stopImmediatePropagation();
    const scan = redisScanPreview;
    const generation = scan.generation;
    scan.scanning = true;
    render();
    setTimeout(() => {
      // 条件变化后，旧扫描的迟到回包不得追加到新的 key 集合。
      if (generation !== scan.generation) return;
      redisScanAdvance();
      scan.scanning = false;
      render();
    }, 180);
    return;
  }
  const button = event.target.closest('[data-result-page]');
  if (!button || button.disabled) return;
  event.stopImmediatePropagation();
  const page = resultPageState(button.dataset.resultId);
  if (button.dataset.resultPage === 'count') page.counted = true;
  else {
    page.page += button.dataset.resultPage === 'next' ? 1 : -1;
    clearResultSelection();
  }
  render();
}, true);
document.addEventListener('change', event => {
  if (event.target.id === 'preview-state') {
    resultPages.clear();
    if (event.target.value in resultPreviewLabels) {
      event.stopImmediatePropagation();
      state.previewEmpty = false;
      state.overlay = null;
      state.menu = null;
      render();
    }
    return;
  }
  const id = event.target.dataset.resultSize;
  if (!id) return;
  event.stopImmediatePropagation();
  const page = resultPageState(id);
  page.size = Number(event.target.value);
  page.page = 0;
  clearResultSelection();
  render();
}, true);

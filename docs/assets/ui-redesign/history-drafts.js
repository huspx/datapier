'use strict';
const historySamples = [
  {
    sql: "SELECT id, order_no, status, total FROM orders WHERE status = 'paid';",
    time: '2026-10-28 09:46:12',
    status: 'success',
  },
  {
    sql: "SELECT id, name, email FROM customers WHERE tier = 'pro';",
    time: '2026-10-28 09:32:08',
    status: 'success',
  },
  {
    sql: 'SELECT order_id, SUM(quantity) FROM order_items GROUP BY order_id;',
    time: '2026-10-27 17:18:45',
    status: 'success',
  },
  {
    sql: 'SELECT * FROM missing_table;',
    time: '2026-10-27 16:05:31',
    status: 'error',
  },
];
for (let n = 5; n <= 125; n++) {
  historySamples.push({
    sql: `SELECT ${n} AS local_example;`,
    time: `2026-10-26 10:${String(n % 60).padStart(2, '0')}:00`,
    status: n % 7 ? 'success' : 'error',
  });
}
function historyPanel(platform = false) {
  const rows = platform
    ? ahistory().map((r, index) => ({
      index,
      sql: r.text,
      detail: `${r.instance} / ${r.db}`,
    }))
    : historySamples.map((r, index) => ({
      index,
      sql: r.sql,
      status: r.status,
      detail: `${r.time} · ${r.status === 'success' ? t('successSample') : t('errorSample')}`,
    })).filter(r =>
      `${r.sql} ${r.detail} ${r.status}`.toLowerCase().includes(
        (state.historyApplied ?? '').toLowerCase(),
      )
    );
  const pageNo = platform ? ap.page : state.historyPage,
    size = platform ? ap.pageSize : state.historyPageSize;
  const page = rows.slice(pageNo * size, (pageNo + 1) * size);
  const selected = platform
    ? page[ap.selectedHistory]
    : page.find(r => r.index === state.historySelected) || page[0];
  if (!platform && selected) state.historySelected = selected.index;
  const queried = platform
    ? ap.historyApplied !== null
    : true;
  const control = platform ? 'data-af="filter"' : 'id="history-filter"';
  const action = platform ? 'data-archery' : 'data-action';
  const query = platform ? 'history-query' : 'local-history-query';
  return `<div class="toolbar"><input type="search" ${control} aria-label="${t('searchQueryHistory')}" placeholder="${
    t('searchSQLTimeOrStatus')
  }" value="${h(platform ? ap.filter : state.historyFilter)}"><button ${action}="${query}">${
    az('查询', 'Search')
  }</button><span class="muted small">${
    platform
      ? az(
        '平台历史 · 当前账号可见',
        'Platform history · visible to current account',
      )
      : az('当前工作区本地历史', 'Current workspace local history')
  }</span></div><div class="history-columns"><div class="history-list">${
    page.map((r, n) =>
      `<button class="${selected === r ? 'active' : ''}" ${
        platform
          ? `data-archery="history-select:${n}"`
          : `data-history="${r.index}"`
      }><span class="mono">${h(r.sql)}</span><br><span class="muted small">${h(r.detail)}</span></button>`
    ).join('') || `<div class="empty">${
      queried
        ? az('没有匹配的历史记录', 'No matching history')
        : az('请输入条件并查询', 'Enter criteria and search')
    }</div>`
  }</div><div class="history-preview"><div class="row"><strong>${
    t('sqlPreview')
  }</strong></div><p class="muted small">${h(selected?.detail || '')}</p><pre class="value">${
    h(selected?.sql || '')
  }</pre></div></div><div class="table-footer"><span>${az('本页', 'Page')}: ${page.length} · ${
    az('总数', 'Total')
  }: ${rows.length}</span><span class="spacer"></span><label class="page-size-control">${
    az('每页', 'Per page')
  } <select ${platform ? 'data-page-size' : 'id="local-history-size"'} aria-label="${
    az('历史记录每页行数', 'History page size')
  }">${
    resultPageSizes.map(n => `<option value="${n}" ${n === size ? 'selected' : ''}>${n}</option>`).join('')
  }</select></label>${
    (platform ? aib : ib)(
      'back',
      t('previousPage'),
      platform ? 'previous' : 'local-history-prev',
      pageNo ? '' : 'disabled',
    )
  }<span class="mono">${
    az(
      `第 ${pageNo + 1} / ${Math.max(1, Math.ceil(rows.length / size))} 页`,
      `Page ${pageNo + 1} / ${Math.max(1, Math.ceil(rows.length / size))}`,
    )
  }</span>${
    (platform ? aib : ib)(
      'chev-r',
      t('nextPage'),
      platform ? 'next' : 'local-history-next',
      (pageNo + 1) * size < rows.length ? '' : 'disabled',
    )
  }</div>`;
}
function currentDrafts() {
  if (state.provider === 'redis') {
    const tab = state.tabs.find(item => item.id === state.active);
    const batch = tab?.redisKeyDraft;
    return [
      ...state.drafts.filter(d => d.db === state.redisDb && d.key === state.key),
      ...(batch ? [batch] : []),
    ];
  }
  return [
    ...(state.newRowDrafts || []).filter(d => d.database === state.database && d.table === state.table),
    ...(databases[state.database]?.[state.table] || []).flatMap(row => [
      ...[...(cellOriginals.get(row) || [])].map(([column, before]) => ({
        row,
        column,
        before,
        after: row[column],
        label: `${state.database}.${state.table}[${rowId(row)}].${column}`,
      })),
      ...(deletedRows.has(row)
        ? [{
          row,
          deletion: true,
          before: { ...row },
          after: t('deletedDraft'),
          label: `${state.database}.${state.table}[${rowId(row)}]`,
        }]
        : []),
    ]),
  ];
}
// 删除资源前检查其所有示例行，包括尚未完成的原位编辑。
function resourceHasDrafts(database, table) {
  const tables = databases[database];
  if (!tables) return false;
  const rows = table ? (tables[table] || []) : Object.values(tables).flat();
  return (state.newRowDrafts || []).some(d => d.database === database && (!table || d.table === table))
    || rows.some(row =>
      cellOriginals.get(row)?.size || deletedRows.has(row)
      || state.cellEdit?.row === row
    );
}
function connectionHasDrafts(provider) {
  return provider === 'mysql'
    ? Object.keys(databases).some(database => resourceHasDrafts(database))
    : state.drafts.length > 0 || state.tabs.some(tab => tab.redisKeyDraft);
}
function draftBar() {
  const count = currentDrafts().reduce(
    (total, draft) => total + (draft.keys?.length || 1),
    0,
  );
  return `<div id="draft-bar" class="toolbar ${count ? '' : 'hidden'}"><strong class="warn">${
    t('pending')
  } ${count}</strong><span class="muted small">${
    t('inMemoryOnly')
  }</span><span class="spacer"></span><button data-action="draft-discard">${
    t('discard')
  }</button><button class="button" data-action="draft-preview">${
    t('previewChanges')
  }</button><button class="primary" data-action="draft-commit">${t('demonstrateSubmit')}</button></div>`;
}
function refreshDraftBar() {
  const bar = $('#draft-bar');
  if (bar) bar.outerHTML = draftBar();
}

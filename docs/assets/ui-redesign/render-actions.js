'use strict';
function render() {
  const previousBody = document.querySelector('#app .modal-body');
  const previousOwner = previousBody?.dataset.previewOwner;
  const previousScroll = previousBody?.scrollTop;
  // 在同一分区重绘加载、失败与语言状态时保留阅读位置；切换分区从顶部开始。
  const restoreDialogScroll = () => {
    const body = document.querySelector('#app .modal-body');
    const owner = mcp.open
      ? `mcp:${mcp.tab}:${mcp.authHandoff}`
      : state.overlay === 'app-settings'
      ? `settings:${configPreview.tab}`
      : state.overlay;
    if (body && owner) {
      body.dataset.previewOwner = owner;
      if (owner === previousOwner) body.scrollTop = previousScroll;
    }
  };
  if (previewSource === 'archery') {
    renderArchery();
    renderMcpPreview();
    restoreDialogScroll();
    return;
  }
  const focused = document.activeElement;
  const focusId = focused?.id;
  const cursor = focused instanceof HTMLInputElement && focused.type !== 'checkbox'
    ? focused.selectionStart
    : null;
  if (state.previewEmpty) state.provider = null;
  else if (!state.connections.has(state.provider)) {
    state.provider = [...state.connections][0] ?? null;
  }
  const app = $('#app');
  app.className = 'shell' + (state.compact ? ' compact' : '');
  app.innerHTML = `${sidebar()}<main class="main">${state.provider ? context() : ''}<div class="workarea">${
    !state.provider
      ? `<div class="empty">${t('noConnections')}</div>`
      : !state.tabs.length
      ? mysql()
      : state.provider === 'mysql'
      ? mysql()
      : tabs() + redis()
  }</div><footer class="statusbar"><span>${
    !state.provider ? t('noConnections') : state.provider === 'mysql'
      ? 'MySQL · ' + h(state.database)
      : 'Redis · DB ' + state.redisDb
  }</span><span class="secondary">${
    !state.provider ? '' : state.provider === 'mysql'
      ? (queryTab() ? t('editorStatus') : t('tableStatus'))
      : t('scanStatus')
  }</span><span class="spacer"></span><span>${t('ready')}</span>${
    state.provider
      ? `<button class="row" data-action="details">${icon('info')} ${t('details')}</button>`
      : ''
  }${
    state.provider === 'redis'
      ? `<button class="row" data-action="cli-toggle">${icon('terminal')} CLI ${
        state.cli ? icon('chev') : icon('chev-up')
      }</button>`
      : ''
  }</footer></main>${overlay()}${archeryWizard()}${archeryDialogs()}${menu()}`;
  renderMcpPreview();
  restoreDialogScroll();
  requestAnimationFrame(() => {
    fitGrid();
    paintCellSelection();
  });
  // 系统语言变化会重绘弹窗；仅恢复同一弹窗的一次输入快照。
  const form = state.languageForm;
  state.languageForm = null;
  if (form?.owner === state.overlay) {
    for (const saved of form.fields) {
      const field = document.getElementById(saved.id);
      if (field) {
        field.value = saved.value;
        if (field.type === 'checkbox') field.checked = saved.checked;
      }
    }
  }
  $('#preview-provider').value = state.provider;
  for (const option of $('#preview-provider').options) {
    option.disabled = !state.connections.has(option.value);
  }
  if ($('#sql-query')) {
    const tab = queryTab();
    $('#sql-query').setSelectionRange(
      tab.sqlStart,
      tab.sqlEnd,
      tab.sqlDirection,
    );
    syncSqlEditor();
  }
  const configAttributes = [
    'data-config-action',
    'data-config-field',
    'data-config-tab',
    'data-config-key',
    'data-config-profile',
    'data-config-policy-owner',
    'data-config-level',
  ]
    .filter(name => focused?.hasAttribute(name));
  const configSelector = configAttributes.map(name => `[${name}="${CSS.escape(focused.getAttribute(name))}"]`).join('');
  const replacement = focusId
    ? document.getElementById(focusId)
    : configSelector
    ? document.querySelector(configSelector)
    : null;
  if (replacement && (!state.overlay || focused?.closest('.modal'))) {
    replacement.focus({ preventScroll: true });
    if (
      cursor !== null && replacement instanceof HTMLInputElement
      && ['text', 'search'].includes(replacement.type)
    ) replacement.setSelectionRange(cursor, cursor);
  }
}
function toast(text) {
  document.querySelector('.toast')?.remove();
  const node = document.createElement('div');
  node.className = 'toast';
  node.role = 'status';
  node.textContent = text;
  $('#app').append(node);
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.remove(), 3500);
}
function selectRow(id, event) {
  const add = isMac ? event.metaKey : event.ctrlKey;
  if (event.shiftKey && state.anchor !== null) {
    const ids = currentRows().map(rowId);
    const from = ids.indexOf(state.anchor);
    const to = ids.indexOf(id);
    if (!add) state.selected.clear();
    for (let i = Math.min(from, to); i <= Math.max(from, to); i++) {
      if (ids[i] !== undefined) state.selected.add(ids[i]);
    }
  } else if (add) {
    state.selected.has(id)
      ? state.selected.delete(id)
      : state.selected.add(id);
    state.anchor = id;
  } else {
    state.selected = new Set([id]);
    state.anchor = id;
  }
  render();
}
function openTable(database, table) {
  if (!databases[database]?.[table]) {
    toast(t('resourceMissing'));
    return;
  }
  let tab = state.tabs.find(t => !t.query && t.database === database && t.table === table);
  if (!tab) {
    tab = { id: state.nextTab++, database, table, title: table };
    state.tabs.push(tab);
  }
  state.active = tab.id;
  state.database = database;
  state.table = table;
  state.selected.clear();
  state.anchor = null;
  state.rowFilter = '';
  state.section = 'data';
  render();
}
async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast(t('copied'));
  } catch {
    toast(t('clipboardPreview'));
  }
}
async function sqlMenuAction(action) {
  const menuState = state.menu, tab = queryTab(), input = $('#sql-query');
  if (
    !tab || !input || menuState?.kind !== 'sql'
    || menuState.tabId !== tab.id
  ) return;
  const { start, end } = menuState;
  const original = tab.sql, selected = original.slice(start, end);
  state.menu = null;
  document.querySelector('.context-menu')?.remove();
  input.focus();
  input.setSelectionRange(start, end);
  tab.sqlStart = start;
  tab.sqlEnd = end;
  if (action === 'sql-menu-execute') return requestSql('selection');
  if (action === 'sql-menu-explain') return requestSql('explain-selection');
  if (action === 'sql-menu-format') {
    try {
      const parsed = scanSql(selected);
      if (parsed.error) throw new Error(parsed.error);
      if (!parsed.statements.length) throw new Error(t('noFormatSql'));
      if (!window.sqlFormatter?.format) {
        throw new Error(t('formatterUnavailable'));
      }
      const leading = selected.match(/^\s*/)[0];
      const trailing = selected.match(/\s*$/)[0];
      const formatted = leading
        + window.sqlFormatter.format(selected.trim(), {
          language: 'mysql',
          keywordCase: 'upper',
          tabWidth: 2,
          linesBetweenQueries: 1,
        }) + trailing;
      const formattedParsed = scanSql(formatted);
      const semicolons = tokens => tokens.filter(t => !t.kind && t.text === ';').length;
      const endsWithSemicolon = tokens => {
        const last = tokens.filter(t => t.kind !== 'comment' && t.text.trim()).at(-1);
        return Boolean(last && !last.kind && last.text === ';');
      };
      if (
        formattedParsed.error
        || semicolons(parsed.tokens) !== semicolons(formattedParsed.tokens)
        || endsWithSemicolon(parsed.tokens)
          !== endsWithSemicolon(formattedParsed.tokens)
      ) {
        throw new Error(t('formatBoundary'));
      }
      if (!document.execCommand('insertText', false, formatted)) {
        throw new Error(t('undoUnsupported'));
      }
      input.setSelectionRange(start, start + formatted.length);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      rememberSqlSelection();
      toast(t('formatted'));
    } catch (error) {
      toast(t('formatFailed', { reason: error.message || String(error) }));
    }
    return;
  }
  if (action === 'sql-menu-all') {
    input.setSelectionRange(0, input.value.length);
    rememberSqlSelection();
    return;
  }
  if (action === 'sql-menu-copy' || action === 'sql-menu-cut') {
    try {
      await navigator.clipboard.writeText(selected);
      if (action === 'sql-menu-cut') {
        if (
          queryTab() !== tab || tab.sql !== original
          || $('#sql-query') !== input
          || input.selectionStart !== start || input.selectionEnd !== end
        ) {
          toast(t('copiedNotCut'));
          return;
        }
        input.setRangeText('', start, end, 'end');
        input.dispatchEvent(new Event('input', { bubbles: true }));
        toast(t('cutDone'));
      } else toast(t('copied'));
    } catch {
      toast(t('clipboardDenied'));
    }
  }
}
function insert(exclude) {
  const selected = currentRows().filter(row => state.selected.has(rowId(row)));
  const columns = Object.keys(selected[0] || {}).filter(c => !exclude || c !== 'id');
  const q = value => value === null ? 'NULL' : "'" + String(value).replace(/'/g, "''") + "'";
  state.sqlCopy = 'INSERT INTO `' + state.database + '`.`' + state.table
    + '` (' + columns.map(c => '`' + c + '`').join(', ') + ')\nVALUES\n'
    + selected.map(row => '  (' + columns.map(c => q(row[c])).join(', ') + ')').join(',\n') + ';';
  state.sqlCopyExclude = exclude;
  state.overlay = null;
  state.menu = null;
  render();
  copy(state.sqlCopy);
}
document.addEventListener('dblclick', event => {
  const cell = event.target.closest('#mysql-grid td[data-cell]');
  if (!cell || event.target.id === 'grid-cell-editor') return;
  startCellEdit(cell.closest('tr').dataset.row, cell.dataset.cell);
});
document.addEventListener('focusout', event => {
  // 整页重绘也会触发失焦；只提交仍在当前表格中的真实编辑框。
  if (event.target.id === 'grid-cell-editor' && event.target.isConnected) {
    finishCellEdit(true);
  }
});
function selectRedisItem(setName, anchorName, item, ordered, event) {
  const selected = state[setName];
  const modified = event.metaKey || event.ctrlKey;
  if (event.shiftKey && ordered.includes(state[anchorName])) {
    const a = ordered.indexOf(state[anchorName]), b = ordered.indexOf(item);
    if (!modified) selected.clear();
    ordered.slice(Math.min(a, b), Math.max(a, b) + 1).forEach(value => selected.add(value));
  } else if (modified) {
    selected.has(item) ? selected.delete(item) : selected.add(item);
    state[anchorName] = item;
  } else {
    selected.clear();
    selected.add(item);
    state[anchorName] = item;
  }
}
function openRedisKey(key) {
  if (!key) return;
  state.key = key.name;
  let tab = state.tabs.find(item =>
    item.provider === 'redis' && item.key === key.name
    && item.redisDb === state.redisDb
  );
  if (!tab) {
    tab = {
      id: state.nextTab++,
      provider: 'redis',
      title: key.name,
      key: key.name,
      redisDb: state.redisDb,
    };
    state.tabs.push(tab);
  }
  state.active = tab.id;
  state.redisRow = 0;
  state.selectedEntries.clear();
  state.entryAnchor = null;
  state.redisFormat = 'text';
}

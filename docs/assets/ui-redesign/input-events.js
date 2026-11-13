'use strict';
document.addEventListener('input', event => {
  const target = event.target;
  if (target.id === 'cell-value') {
    state.cellInspectorValue = target.value;
    state.cellInspectorIsNull = false;
    state.cellInspectorDirty = true;
    target.parentElement.querySelector('.null')?.remove();
  }
  if (target.dataset.advancedValue) {
    state.advancedDemoValues ||= {};
    state.advancedDemoValues[target.dataset.advancedValue] = target.value;
  } else if (target.hasAttribute('data-advanced-path')) {
    state.advancedDemoPath = target.value;
  }
});
document.addEventListener('contextmenu', event => {
  const archeryCard = event.target.closest('[data-archery-connection]');
  if (archeryCard) {
    event.preventDefault();
    const area = $('#app').getBoundingClientRect();
    state.menu = {
      kind: 'connection',
      archeryId: archeryCard.dataset.archeryConnection,
      archeryServer: archeryCard.dataset.connectionServer,
      x: Math.max(4, Math.min(event.clientX - area.left, area.width - 280)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - 130)),
    };
    render();
    return;
  }
  const redisKey = event.target.closest('[data-key-index]');
  const redisEntry = event.target.closest('[data-redis-row]');
  if (redisKey || redisEntry) {
    event.preventDefault();
    const key = redisKey
      ? keySamples[Number(redisKey.dataset.keyIndex)]
      : keySamples.find(k => k.name === state.key);
    const item = redisKey ? key : Number(redisEntry.dataset.redisRow);
    const set = redisKey ? state.selectedKeys : state.selectedEntries;
    if (!set.has(item)) {
      set.clear();
      set.add(item);
      if (redisKey) state.keyAnchor = item;
      else {
        state.entryAnchor = item;
        state.redisRow = item;
      }
    }
    const area = $('#app').getBoundingClientRect();
    state.menu = {
      kind: redisKey ? 'redis-key' : 'redis-entry',
      key,
      x: Math.max(4, Math.min(event.clientX - area.left, area.width - 250)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - 160)),
    };
    render();
    return;
  }
  const tabElement = event.target.closest('[data-tab-container]');
  if (tabElement) {
    event.preventDefault();
    const area = $('#app').getBoundingClientRect();
    state.menu = {
      kind: 'tab',
      tabId: Number(tabElement.dataset.tabContainer),
      x: Math.max(4, Math.min(event.clientX - area.left, area.width - 220)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - 140)),
    };
    render();
    return;
  }
  if (event.target.id === 'sql-query') {
    const input = event.target, tab = queryTab();
    const start = input.selectionStart, end = input.selectionEnd;
    if (!tab || !input.value.slice(start, end).trim()) return;
    event.preventDefault();
    tab.sqlStart = start;
    tab.sqlEnd = end;
    const area = $('#app').getBoundingClientRect();
    state.menu = {
      kind: 'sql',
      tabId: tab.id,
      start,
      end,
      x: Math.max(4, Math.min(event.clientX - area.left, area.width - 264)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - 260)),
    };
    document.querySelector('.context-menu')?.remove();
    $('#app').insertAdjacentHTML('beforeend', menu());
    return;
  }
  const connection = event.target.closest('.connection[data-provider]');
  if (connection) {
    event.preventDefault();
    const area = $('#app').getBoundingClientRect();
    state.menu = {
      kind: 'connection',
      provider: connection.dataset.provider,
      x: Math.max(4, Math.min(event.clientX, area.width - 280)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - 130)),
    };
    render();
    return;
  }
  const resource = event.target.closest('[data-open-table], [data-db]');
  if (resource) {
    event.preventDefault();
    const area = $('#app').getBoundingClientRect();
    const menuHeight = resource.dataset.openTable ? 280 : 240;
    state.menu = {
      kind: resource.dataset.openTable ? 'resource' : 'database',
      database: resource.dataset.tableDb || resource.dataset.db,
      table: resource.dataset.openTable,
      x: Math.max(4, Math.min(event.clientX - area.left, area.width - 280)),
      y: Math.max(4, Math.min(event.clientY - area.top, area.height - menuHeight)),
    };
    render();
    return;
  }
  if (event.target.id === 'grid-cell-editor') return;
  const row = event.target.closest('#mysql-grid tbody tr');
  if (!row) return;
  event.preventDefault();
  if (
    state.cellDrawer && state.cellInspectorDirty
    && (row.dataset.row !== state.menuRow
      || (event.target.closest('[data-cell]')?.dataset.cell || state.menuCell) !== state.menuCell)
  ) {
    const rowIdValue = row.dataset.row,
      column = event.target.closest('[data-cell]')?.dataset.cell,
      x = event.clientX,
      y = event.clientY;
    requestDialogClose(() => {
      state.cellDrawer = false;
      state.cellInspectorValue = null;
      render();
      const target = document.querySelector(
        `#mysql-grid tr[data-row="${CSS.escape(rowIdValue)}"] ${
          column ? `td[data-cell="${CSS.escape(column)}"]` : 'td'
        }`,
      );
      target?.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: x, clientY: y }));
    });
    return;
  }
  const id = row.dataset.row;
  if (!state.selected.has(id)) {
    state.selected = new Set([id]);
    state.anchor = id;
  }
  const app = $('#app').getBoundingClientRect();
  state.menuRow = id;
  state.menuCell = event.target.closest('[data-cell]')?.dataset.cell
    || Object.keys(databases[state.database][state.table][0])[0];
  state.menu = {
    cell: !!event.target.closest('[data-cell]'),
    displayText: event.target.closest('td')?.textContent || '',
    x: Math.max(4, Math.min(event.clientX, app.width - 284)),
    y: Math.max(4, Math.min(event.clientY - app.top, app.height - 280)),
  };
  render();
});
document.addEventListener('mousedown', event => {
  if (
    event.button === 2 && event.target.id === 'sql-query'
    && event.target.selectionStart !== event.target.selectionEnd
  ) {
    // 浏览器会在 contextmenu 之前移动光标；先阻止默认选区变化。
    event.preventDefault();
  }
  if (event.target.closest('[data-action^="sql-"]')) event.preventDefault();
});
document.addEventListener('selectionchange', rememberSqlSelection);
document.addEventListener('select', event => {
  if (event.target.id === 'sql-query') rememberSqlSelection();
}, true);
document.addEventListener('keyup', event => {
  if (event.target.id === 'sql-query') rememberSqlSelection();
});
document.addEventListener('scroll', event => {
  if (event.target.id === 'sql-query') syncSqlScroll();
}, true);
document.addEventListener('keydown', event => {
  if (event.key !== 'Enter') return;
  if (event.target.id === 'history-filter') {
    event.preventDefault();
    document.querySelector('[data-action="local-history-query"]')?.click();
  } else if (event.target.matches('[data-af="filter"]')) {
    event.preventDefault();
    document.querySelector('[data-archery="history-query"]')?.click();
  }
}, true);
document.addEventListener('input', event => {
  if (event.target.hasAttribute('aria-invalid')) {
    event.target.removeAttribute('aria-invalid');
    event.target.parentElement.querySelector('.field-error')?.remove();
  }
  const id = event.target.id;
  if (id === 'history-filter') {
    state.historyFilter = event.target.value;
    return;
  }
  if (id === 'grid-cell-editor') {
    if (state.cellEdit) state.cellEdit.value = event.target.value;
    return;
  }
  if (id === 'connection-db-search') {
    state.connectionForm.dbSearch = event.target.value;
    syncDatabaseOptions();
    return;
  }
  if (event.target.dataset.newDatabaseField) {
    state.newDatabaseForm[event.target.dataset.newDatabaseField] = event.target.value;
    updateDatabasePreview();
    return;
  }
  if (event.target.dataset.connectionField) {
    state.connectionForm[event.target.dataset.connectionField] = event.target.value;
    return;
  }
  if (id === 'sql-query') {
    if (state.menu?.kind === 'sql') {
      state.menu = null;
      document.querySelector('.context-menu')?.remove();
    }
    const tab = queryTab();
    tab.sql = event.target.value;
    tab.sqlStart = event.target.selectionStart;
    tab.sqlEnd = event.target.selectionEnd;
    tab.sqlDirection = event.target.selectionDirection;
    tab.execution = null;
    tab.resultFixture = false;
    document.querySelector('.sql-results')?.remove();
    document.querySelector('.sql-workbench')?.classList.remove(
      'has-sql-results',
    );
    syncSqlEditor();
    return;
  }
  if (id === 'result-filter') state.resultFilter = event.target.value;
  else if (id === 'object-filter') state.objectFilter = event.target.value;
  else if (id === 'row-filter') {
    state.rowFilter = event.target.value;
    state.selected.clear();
    state.anchor = null;
  } else if (id === 'key-filter') {
    state.keyFilter = event.target.value;
    state.selectedKeys.clear();
    state.keyAnchor = null;
  } else if (id === 'cli-command') {
    state.cliText = event.target.value;
    return;
  } else return;
  render();
});
document.addEventListener('change', event => {
  const id = event.target.id;
  if (event.target.dataset.connectionCheck) {
    state.connectionForm[event.target.dataset.connectionCheck] = event.target.checked;
    if (event.target.dataset.connectionCheck === 'tls') {
      document.querySelector('.tls-fields').hidden = !event.target.checked;
    }
    return;
  }
  if (id === 'cell-field') {
    const next = event.target.value;
    event.target.value = state.menuCell;
    requestDialogClose(() => {
      state.menuCell = next;
      state.cellInspectorValue = null;
      state.cellInspectorIsNull = databases[state.database][state.table].find(row =>
        rowId(row) === state.menuRow
      )?.[next] === null;
      state.cellInspectorDirty = false;
      render();
      document.getElementById('cell-field')?.focus();
    }, state.cellInspectorDirty);
    return;
  }
  if (id === 'language-preference') {
    captureLanguageInputs();
    languagePreference = event.target.value;
    language = languagePreference === 'system'
      ? systemLanguage()
      : languagePreference;
    let stored = true;
    try {
      localStorage.setItem(languageKey, languagePreference);
    } catch {
      stored = false;
    }
    translatePreview();
    render();
    if (!stored) toast(t('storageUnavailable'));
    return;
  }
  if (event.target.dataset.databaseChoice) {
    const selected = state.connectionForm.selectedDatabases;
    event.target.checked
      ? selected.add(event.target.dataset.databaseChoice)
      : selected.delete(event.target.dataset.databaseChoice);
    syncDatabaseOptions();
    return;
  }
  if (event.target.dataset.newDatabaseField) {
    const key = event.target.dataset.newDatabaseField;
    state.newDatabaseForm[key] = event.target.value;
    if (key === 'charset') {
      state.newDatabaseForm.collation = '';
      const options = databasePreviewCollations[event.target.value];
      document.querySelector('[data-new-database-field="collation"]').innerHTML =
        `<option value="">${az('字符集默认', 'Character set default')}</option>`
        + options.map(value => `<option>${value}</option>`).join('');
    }
    updateDatabasePreview();
    return;
  }
  if (event.target.dataset.connectionField) {
    state.connectionForm[event.target.dataset.connectionField] = event.target.value;
    return;
  }
  if (id === 'preview-theme') {
    document.documentElement.dataset.theme = event.target.value;
    return;
  }
  if (id === 'preview-source') {
    previewSource = event.target.value;
    state.overlay = null;
    state.menu = null;
    ap.modal = false;
    ap.view = 'query';
    serverForm = null;
    serverDelete = null;
    connectionDelete = null;
    authStage = null;
    state.previewEmpty = false;
    state.historyFilter = '';
    state.historyApplied = '';
    ap.filter = '';
    ap.historyApplied = '';
    state.historyPage = 0;
    ap.page = 0;
    $('#preview-state').value = previewSource === 'archery'
      ? 'archery-query'
      : 'default';
    translatePreview();
    render();
    return;
  }
  if (id === 'preview-state' && event.target.value.startsWith('archery-')) {
    selectArcheryPreview(event.target.value);
    render();
    return;
  }
  if (id === 'preview-provider') activateProvider(event.target.value);
  else if (id === 'redis-db') {
    if (currentDrafts().length) {
      event.target.value = state.redisDb;
      toast(t('closeBlocked'));
      return;
    }
    state.redisDb = Number(event.target.value);
    state.cliDb = state.redisDb;
    state.selectedKeys.clear();
    state.keyAnchor = null;
    state.selectedEntries.clear();
    state.entryAnchor = null;
    state.key = state.redisDb === 0 ? keySamples[0].name : '';
    state.cliText = '';
    state.cliHistory = false;
  } else if (id === 'key-type') {
    state.keyType = event.target.value;
    state.selectedKeys.clear();
    state.keyAnchor = null;
  } else if (id === 'preview-state') {
    const kind = event.target.value;
    state.previewEmpty = kind === 'empty-connections';
    state.menu = null;
    state.overlay = null;
    if (state.previewEmpty) {
      render();
      return;
    }
    if (kind.startsWith('sample-')) {
      state.overlay = kind;
      render();
      return;
    }
    if (kind.startsWith('schema-create-')) {
      previewSource = 'direct';
      $('#preview-source').value = 'direct';
      state.provider = 'mysql';
      const type = kind === 'schema-create-table' ? 'new-table' : kind === 'schema-create-view' ? 'view' : 'trigger';
      if (!schemaCreateWorkspace(type)) {
        render();
        toast(
          language === 'zh-CN'
            ? '只读连接或数据库不可用，不能新建对象。'
            : 'Cannot create an object on a read-only connection or without a database.',
        );
      }
      return;
    }
    if (kind === 'schema-trigger-list') {
      previewSource = 'direct';
      $('#preview-source').value = 'direct';
      state.provider = 'mysql';
      const existing = state.tabs.find(tab =>
        !tab.query && !tab.schemaCreateKind && tab.database === 'commerce' && tab.table === 'orders'
      );
      if (existing && schemaHasDraft(existing)) {
        render();
        toast(t('closeBlocked'));
        return;
      }
      openTable('commerce', 'orders');
      if (!schemaOpenExample('trigger-list')) {
        render();
        toast(language === 'zh-CN' ? '无法打开触发器列表。' : 'Unable to open the trigger list.');
      }
      return;
    }
    if (kind === 'schema-design') {
      previewSource = 'direct';
      $('#preview-source').value = 'direct';
      state.provider = 'mysql';
      openTable('commerce', 'orders');
      state.section = 'structure';
      schemaDesignAction('schema-example');
      return;
    }
    if (['advanced-value', 'replace-design'].includes(kind)) {
      if (kind === 'advanced-value') {
        state.cellValueTarget = null;
        state.valuePreviewTarget = null;
      }
      state.overlay = kind;
      state.replaceDemoPlanned = false;
      state.replaceDemoExcluded = false;
      render();
      return;
    }
    if (['query-editor', 'query-result', 'ddl'].includes(kind)) {
      state.provider = 'mysql';
      if (kind === 'ddl') {
        openTable('commerce', 'orders');
        state.section = 'ddl';
      } else {
        let tab = state.tabs.find(t => t.query);
        if (!tab) {
          tab = {
            id: state.nextTab++,
            title: t('defaultQueryFile'),
            query: true,
            database: state.database,
          };
          state.tabs.push(tab);
        }
        state.active = tab.id;
        state.database = tab.database;
        tab.execution = null;
        tab.resultFixture = kind === 'query-result';
      }
      render();
      return;
    }
    if (kind === 'insert-output') {
      if (!state.sqlCopy) {
        toast(t('selectBeforeInsert'));
        return;
      }
      state.provider = 'mysql';
      state.overlay = 'sql-copy';
      render();
      return;
    }
    if (kind.startsWith('connection-')) {
      state.connectionStatus[state.provider] = kind.slice(
        'connection-'.length,
      );
      render();
      return;
    }
    if (kind.startsWith('cli-')) {
      activateProvider('redis');
      state.redisDb = 0;
      state.key = keySamples[0].name;
      state.cli = true;
      state.cliState = kind.slice(4);
      appendCliRecord(state.cliEntries, {
        command: state.cliState === 'error'
          ? 'GET user:1042'
          : state.cliState === 'empty'
          ? 'GET missing:key'
          : state.cliText,
        database: String(state.redisDb),
        instance: 'Cache',
        status: state.cliState === 'error'
          ? 'error'
          : state.cliState === 'running'
          ? 'running'
          : 'success',
        ms: 1,
        text: state.cliState === 'error'
          ? 'WRONGTYPE Operation against a key holding the wrong kind of value'
          : state.cliState === 'empty'
          ? '(nil)'
          : state.cliState === 'running'
          ? t('waitingServer')
          : t('cliPreview'),
      });
    } else if (kind !== 'default') {
      state.provider = 'mysql';
      state.detailKind = kind;
      state.overlay = 'details';
    }
  } else return;
  render();
});
function syncMysqlTree() {
  const nodes = [
    ...document.querySelectorAll(
      '.nav-content [data-db], .nav-content [data-tree-group], .nav-content [data-open-table], .nav-content [data-view-preview]',
    ),
  ];
  nodes.forEach((node, index) => {
    node.setAttribute('role', 'treeitem');
    node.tabIndex = index ? -1 : 0;
  });
  if (nodes.length) nodes[0].parentElement.setAttribute('role', 'tree');
}
document.addEventListener('focusin', event => {
  const item = event.target.closest?.('.nav-content [role="treeitem"]');
  if (!item) return;
  item.parentElement.querySelectorAll('[role="treeitem"]').forEach(node => {
    node.tabIndex = node === item ? 0 : -1;
  });
});
document.addEventListener('keydown', event => {
  const item = event.target.closest?.('.nav-content [role="treeitem"]');
  if (!item || !['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  const nodes = [...item.parentElement.querySelectorAll('[role="treeitem"]')];
  const index = nodes.indexOf(item), expanded = item.getAttribute('aria-expanded');
  event.preventDefault();
  if ((event.key === 'ArrowRight' && expanded === 'false') || (event.key === 'ArrowLeft' && expanded === 'true')) {
    const selector = item.dataset.db
      ? `[data-db="${CSS.escape(item.dataset.db)}"]`
      : `[data-tree-group="${CSS.escape(item.dataset.treeGroup)}"]`;
    item.click();
    document.querySelector(selector)?.focus();
    return;
  }
  let next = index;
  if (event.key === 'ArrowDown' || event.key === 'ArrowRight' && expanded === 'true') {
    next = Math.min(nodes.length - 1, index + 1);
  }
  if (event.key === 'ArrowUp') next = Math.max(0, index - 1);
  if (event.key === 'Home') next = 0;
  if (event.key === 'End') next = nodes.length - 1;
  if (event.key === 'ArrowLeft' && expanded !== 'true') {
    const depth = item.classList.contains('depth-2') ? 2 : item.classList.contains('depth-1') ? 1 : 0;
    for (let i = index - 1; depth && i >= 0; i--) {
      const parentDepth = nodes[i].classList.contains('depth-2') ? 2 : nodes[i].classList.contains('depth-1') ? 1 : 0;
      if (parentDepth < depth) {
        next = i;
        break;
      }
    }
  }
  nodes[next]?.focus();
});
document.addEventListener('keydown', event => {
  if (
    state.menu
    && ['ArrowDown', 'ArrowUp', 'Enter', 'Escape'].includes(event.key)
  ) {
    event.preventDefault();
    const items = [
      ...document.querySelectorAll(
        '.context-menu [role="menuitem"]:not(:disabled)',
      ),
    ];
    if (event.key === 'Escape') {
      state.menu = null;
      render();
      return;
    }
    if (event.key === 'Enter') {
      if (items.includes(document.activeElement)) {
        document.activeElement.click();
      }
      return;
    }
    const current = items.indexOf(document.activeElement);
    const index = current < 0
      ? (event.key === 'ArrowDown' ? 0 : items.length - 1)
      : (current + (event.key === 'ArrowDown' ? 1 : -1) + items.length)
        % items.length;
    items[index]?.focus();
    return;
  }
  if (event.target.id === 'grid-cell-editor') {
    if (event.isComposing) return;
    if (['Enter', 'Escape', 'Tab'].includes(event.key)) {
      event.preventDefault();
      const cell = event.target.closest('td');
      const cells = [
        ...document.querySelectorAll('#mysql-grid td[data-cell]'),
      ];
      const index = cells.indexOf(cell);
      finishCellEdit(event.key !== 'Escape');
      if (event.key === 'Tab') {
        const next = cells[
          Math.max(
            0,
            Math.min(cells.length - 1, index + (event.shiftKey ? -1 : 1)),
          )
        ];
        if (next) {
          startCellEdit(next.closest('tr').dataset.row, next.dataset.cell);
          if (!state.cellEdit) next.focus();
        }
      } else cell.focus();
    }
    return;
  }
  const cell = event.target.closest('#mysql-grid td[data-cell]');
  if (cell && ['Enter', 'F2'].includes(event.key)) {
    event.preventDefault();
    startCellEdit(cell.closest('tr').dataset.row, cell.dataset.cell);
    return;
  }
  if (
    event.target.dataset.redisRowNumber !== undefined
    && ['Enter', ' '].includes(event.key)
  ) {
    event.preventDefault();
    state.redisRow = Number(event.target.dataset.redisRowNumber);
    selectRedisItem(
      'selectedEntries',
      'entryAnchor',
      state.redisRow,
      [...document.querySelectorAll('[data-redis-row]')].map(row => Number(row.dataset.redisRow)),
      event,
    );
    render();
    return;
  }
  if (
    event.target.dataset.selectRow && ['Enter', ' '].includes(event.key)
  ) {
    event.preventDefault();
    selectRow(event.target.dataset.selectRow, event);
    return;
  }
  if (
    event.target.matches('.provider-options [role="radio"]')
    && ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)
  ) {
    const choices = [
      ...event.target.closest('.provider-options').querySelectorAll(
        '[role="radio"]:not(:disabled):not([aria-disabled="true"])',
      ),
    ];
    const index = choices.indexOf(event.target);
    if (index >= 0 && choices.length) {
      event.preventDefault();
      const next = event.key === 'Home' ? 0 : event.key === 'End'
        ? choices.length - 1
        : (index + (['ArrowRight', 'ArrowDown'].includes(event.key) ? 1 : -1) + choices.length) % choices.length;
      const choice = choices[next];
      choice.click();
      if (!document.querySelector('[data-discard-confirmation]')) {
        document.querySelector('.provider-options [role="radio"][aria-checked="true"]')?.focus();
      }
    }
    return;
  }
  if (
    event.target.id === 'sql-query' && event.key === 'Enter'
    && (event.metaKey || event.ctrlKey)
  ) {
    event.preventDefault();
    rememberSqlSelection();
    const tab = queryTab();
    requestSql(
      event.shiftKey
        ? 'all'
        : tab.sql.slice(tab.sqlStart, tab.sqlEnd).trim()
        ? 'selection'
        : 'current',
    );
    return;
  }
  if (event.key === 'Escape') {
    if (document.querySelector('[data-discard-confirmation]')) {
      document.querySelector('[data-continue-editing]')?.click();
    } else if (state.menu) {
      state.menu = null;
      render();
    } else if (serverForm || serverDelete || connectionDelete || authStage) {
      requestDialogClose(() => {
        if (authStage) {
          closeArcheryAuth();
        } else {
          serverForm = null;
          serverDelete = null;
          connectionDelete = null;
        }
        render();
      });
    } else if (state.overlay) {
      requestDialogClose(closePreviewDialog);
    } else if (ap.modal) {
      requestDialogClose(() => {
        ap.modal = false;
        render();
      });
    } else if (state.cellDrawer) {
      requestDialogClose(() => {
        state.cellDrawer = false;
        state.cellInspectorValue = null;
        render();
      });
    } else if (previewSource === 'archery' && ap.view === 'history') {
      ap.view = ap.historyReturn || 'query';
      render();
    }
  }
  if (event.target.id === 'cli-command' && event.key === 'Enter') {
    event.preventDefault();
    document.querySelector('[data-action="cli-run"]')?.click();
  }
  if (
    event.target.matches('[data-af="command"]') && event.key === 'Enter'
    && previewSource === 'archery'
  ) {
    event.preventDefault();
    document.querySelector('[data-archery="result"]')?.click();
  }
});

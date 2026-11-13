'use strict';
document.addEventListener('click', event => {
  const folder = event.target.closest('[data-key-folder]');
  if (folder) {
    const prefix = decodeURIComponent(folder.dataset.keyFolder);
    const identity = state.redisDb + '\0' + prefix;
    if (state.expandedKeyFolders.has(identity)) {
      for (const key of state.selectedKeys) if (key.name.startsWith(prefix)) state.selectedKeys.delete(key);
      if (state.keyAnchor?.name?.startsWith(prefix)) state.keyAnchor = null;
    }
    state.expandedKeyFolders.has(identity)
      ? state.expandedKeyFolders.delete(identity)
      : state.expandedKeyFolders.add(identity);
    render();
    return;
  }
  const clickedCell = event.target.closest(
    '#mysql-grid td[data-cell], #result-grid td[data-cell], #redis-grid td[data-cell]',
  );
  if (clickedCell && event.target.id !== 'grid-cell-editor') {
    if (
      state.cellDrawer && state.cellInspectorDirty && clickedCell.closest('#mysql-grid')
      && (clickedCell.closest('tr').dataset.row !== state.menuRow || clickedCell.dataset.cell !== state.menuCell)
    ) {
      const row = clickedCell.closest('tr').dataset.row, column = clickedCell.dataset.cell;
      requestDialogClose(() => {
        state.cellDrawer = false;
        state.cellInspectorValue = null;
        render();
        document.querySelector(`#mysql-grid tr[data-row="${CSS.escape(row)}"] td[data-cell="${CSS.escape(column)}"]`)
          ?.click();
      });
      return;
    }
    const redis = clickedCell.closest('#redis-grid'),
      result = clickedCell.closest('#result-grid');
    const key = redis
      ? `redis:${state.redisDb}:${state.key}`
      : result
      ? `result:${state.active}`
      : `${state.database}.${state.table}`;
    const position = gridPosition(clickedCell);
    if (event.shiftKey && state.cellSelection?.key === key) {
      state.cellSelection.end = position;
    } else state.cellSelection = { key, start: position, end: position };
    state.selected.clear();
    state.selectedEntries.clear();
    if (redis) {
      state.redisRow = Number(clickedCell.closest('tr').dataset.redisRow);
    } else if (!result) {
      state.menuRow = clickedCell.closest('tr').dataset.row;
      state.menuCell = clickedCell.dataset.cell;
      const details = document.querySelector(
        '[data-action="inspect-cell"]',
      );
      if (details) details.disabled = false;
    }
    clickedCell.focus();
    paintCellSelection();
    return;
  }
  const redisRow = event.target.closest('[data-redis-row-number]')?.closest(
    '[data-redis-row]',
  );
  if (redisRow) {
    state.cellSelection = null;
    state.redisRow = Number(redisRow.dataset.redisRow);
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
  const rowNumber = event.target.closest('[data-select-row]');
  if (rowNumber) {
    state.cellSelection = null;
    selectRow(rowNumber.dataset.selectRow, event);
    return;
  }
  const button = event.target.closest('button');
  if (!button) {
    if (state.menu) {
      state.menu = null;
      document.querySelector('.context-menu')?.remove();
    }
    return;
  }
  if (button.dataset.viewPreview) {
    state.viewPreviewTarget = { database: button.dataset.viewDb, name: button.dataset.viewPreview };
    state.overlay = 'view-resource-preview';
    render();
    return;
  }
  if (button.dataset.provider) {
    if (state.connectionStatus[button.dataset.provider] !== 'connected') {
      state.connectionStatus[button.dataset.provider] = 'connected';
    }
    activateProvider(button.dataset.provider);
    state.menu = null;
    render();
    return;
  }
  if (button.dataset.db) {
    state.expanded.has(button.dataset.db)
      ? state.expanded.delete(button.dataset.db)
      : state.expanded.add(button.dataset.db);
    render();
    return;
  }
  if (button.dataset.openTable) {
    openTable(button.dataset.tableDb, button.dataset.openTable);
    return;
  }
  if (button.dataset.tab) {
    if (state.cellDrawer && Number(button.dataset.tab) !== state.active) {
      const tabId = button.dataset.tab;
      leaveCellDrawer(() => document.querySelector(`[data-tab="${CSS.escape(tabId)}"]`)?.click());
      return;
    }
    state.active = Number(button.dataset.tab);
    const tab = state.tabs.find(t => t.id === state.active);
    state.provider = tab.provider || 'mysql';
    state.database = tab.database || state.database;
    if (tab.table) state.table = tab.table;
    if (tab.key) {
      state.key = tab.key;
      state.redisDb = tab.redisDb;
      state.selectedEntries.clear();
      state.entryAnchor = null;
    }
    state.selected.clear();
    state.anchor = null;
    state.rowFilter = '';
    render();
    return;
  }
  if (button.dataset.section) {
    state.section = button.dataset.section;
    render();
    return;
  }
  if (button.dataset.keyIndex !== undefined) {
    const key = keySamples[Number(button.dataset.keyIndex)];
    const ordered = [...document.querySelectorAll('[data-key-index]')].map(
      row => keySamples[Number(row.dataset.keyIndex)],
    );
    selectRedisItem('selectedKeys', 'keyAnchor', key, ordered, event);
    if (!event.shiftKey && !event.metaKey && !event.ctrlKey) {
      openRedisKey(key);
    }
    render();
    return;
  }
  if (button.dataset.history !== undefined) {
    state.historySelected = Number(button.dataset.history);
    render();
    return;
  }
  if (button.dataset.command) {
    state.cliText = button.dataset.command;
    render();
    $('#cli-command')?.focus();
    return;
  }
  if (button.dataset.connectionType) {
    const form = state.connectionForm;
    if (form.provider !== button.dataset.connectionType) {
      requestDialogClose(() => {
        const oldPort = form.provider === 'mysql' ? '3306' : '6379';
        form.provider = button.dataset.connectionType;
        if (form.port === oldPort) form.port = form.provider === 'mysql' ? '3306' : '6379';
        render();
        document.querySelector(`[data-connection-type="${form.provider}"]`)?.focus();
      });
    }
    return;
  }
  const action = button.dataset.action;
  if (!action) return;
  if (action === 'schema-trigger-list') {
    const target = state.menu;
    state.menu = null;
    if (!target?.table) return;
    const existing = state.tabs.find(tab =>
      !tab.query && !tab.schemaCreateKind && tab.database === target.database && tab.table === target.table
    );
    if (existing && schemaHasDraft(existing)) {
      render();
      toast(t('closeBlocked'));
      return;
    }
    openTable(target.database, target.table);
    if (!schemaOpenExample('trigger-list')) {
      render();
      toast(language === 'zh-CN' ? '无法打开触发器列表。' : 'Unable to open the trigger list.');
    }
    return;
  }
  if (action.startsWith('schema-create:')) {
    const database = state.menu?.database;
    state.menu = null;
    if (database) state.database = database;
    if (!schemaCreateWorkspace(action.slice('schema-create:'.length))) {
      render();
      toast(
        language === 'zh-CN'
          ? '只读连接或数据库不可用，不能新建对象。'
          : 'Cannot create an object on a read-only connection or without a database.',
      );
    }
    return;
  }
  if (action.startsWith('advanced-demo:')) {
    state.advancedDemoMode = action.slice('advanced-demo:'.length);
    render();
    return;
  }
  if (schemaDesignAction(action)) {
    return;
  }
  if (action === 'replace-design-demo') {
    state.replaceDemoPlanned = !state.replaceDemoPlanned;
    state.replaceDemoExcluded = false;
    render();
    return;
  }
  if (action === 'replace-design-exclude') {
    state.replaceDemoExcluded = !state.replaceDemoExcluded;
    render();
    return;
  }
  if (action === 'shortcut-help') {
    state.menu = null;
    state.overlay = 'shortcut-help';
    render();
    return;
  }
  if (action === 'local-history-prev' || action === 'local-history-next') {
    state.historyPage += action.endsWith('prev') ? -1 : 1;
    render();
    return;
  }
  if (action === 'local-history-query') {
    state.historyApplied = state.historyFilter;
    state.historyPage = 0;
    state.historySelected = 0;
    render();
    return;
  }
  if (action === 'history') {
    state.overlay = 'history';
    render();
    return;
  }
  if (action === 'open-history') {
    const tab = {
      id: state.nextTab++,
      database: state.database,
      query: true,
      title: t('queryFile', { number: state.nextTab }),
      sql: historySamples[state.historySelected].sql,
    };
    state.tabs.push(tab);
    state.active = tab.id;
    state.database = tab.database;
    state.overlay = null;
    render();
    return;
  }
  if (action.startsWith('format-')) {
    state.redisFormat = action.slice(7);
    render();
    return;
  }
  if (
    ['table-columns', 'result-columns', 'redis-columns'].includes(action)
  ) {
    const key = keySamples.find(item => item.name === state.key);
    state.columnsTarget = action === 'table-columns'
      ? `${state.database}.${state.table}`
      : action === 'result-columns'
      ? 'result'
      : 'redis:' + key.type;
    state.overlay = 'columns';
    render();
    return;
  }
  if (action === 'apply-columns') {
    const widths = [...document.querySelectorAll('[data-column-width]')];
    const invalidWidth = widths.find(input =>
      input.value
      && (!/^\d+$/.test(input.value) || Number(input.value) < 1
        || Number(input.value) > 65535)
    );
    if (invalidWidth) {
      fieldError(
        invalidWidth,
        language === 'zh-CN'
          ? '请输入 1–65535 的宽度。'
          : 'Enter a width from 1 to 65535.',
      );
      return;
    }
    state.columnWidths[state.columnsTarget] = Object.fromEntries(
      widths.filter(input => input.value).map(
        input => [input.dataset.columnWidth, Number(input.value)],
      ),
    );
    state.hiddenColumns[state.columnsTarget] = [
      ...document.querySelectorAll('[data-column-choice]'),
    ].filter(input => !input.checked).map(input => input.dataset.columnChoice);
    state.frozenColumns ||= {};
    state.frozenColumns[state.columnsTarget] = [...document.querySelectorAll('[data-column-freeze]:checked')].map(
      input => input.dataset.columnFreeze,
    );
    state.overlay = null;
    render();
    return;
  }
  if (action.startsWith('redis-menu-')) {
    const menu = state.menu;
    if (action === 'redis-menu-open') openRedisKey(menu.key);
    if (action === 'redis-menu-copy') {
      const value = menu.kind === 'redis-key'
        ? redisKeyClipboard([...state.selectedKeys])
        : redisSelectedClipboard(menu.key, state.selectedEntries);
      copy(value);
    }
    if (action === 'redis-menu-delete') {
      state.redisDeleteTarget = menu.kind === 'redis-key'
        ? {
          db: state.redisDb,
          tabId: state.active,
          keys: [...state.selectedKeys],
        }
        : {
          db: state.redisDb,
          tabId: state.active,
          key: menu.key,
          entries: [...state.selectedEntries].sort((a, b) => a - b).map(
            index => ({ index, row: [...menu.key.rows[index]] }),
          ),
        };
      state.overlay = menu.kind === 'redis-key'
        ? 'delete-key'
        : 'delete-entry';
    }
    state.menu = null;
    render();
    return;
  }
  if (
    [
      'filter',
      'new-row',
      'export',
      'import',
      'transfer',
      'query-file',
      'ttl',
      'rename-key',
      'delete-key',
      'edit-key',
      'add-entry',
      'delete-entry',
      'draft-preview',
    ].includes(action)
  ) {
    if (['import', 'transfer'].includes(action)) state.importPreview = null;
    if (action === 'delete-key') {
      state.redisDeleteTarget = {
        db: state.redisDb,
        tabId: state.active,
        single: true,
        keys: [keySamples.find(k => k.name === state.key)],
      };
    }
    if (action === 'delete-entry') {
      const key = keySamples.find(k => k.name === state.key);
      state.redisDeleteTarget = {
        db: state.redisDb,
        tabId: state.active,
        key,
        entries: [...state.selectedEntries].sort((a, b) => a - b).map(
          index => ({ index, row: [...key.rows[index]] }),
        ),
      };
    }
    state.overlay = action;
    render();
    return;
  }
  if (action === 'stage-new-row') {
    let values = {};
    const advanced = document.getElementById('new-row-json');
    if (advanced.value.trim()) {
      try {
        values = JSON.parse(advanced.value);
        if (!values || Array.isArray(values) || typeof values !== 'object') throw new Error();
      } catch {
        fieldError(advanced, language === 'zh-CN' ? '请输入 JSON 对象。' : 'Enter a JSON object.');
        return;
      }
    } else {
      for (const field of document.querySelectorAll('[data-new-column]')) {
        const mode = field.querySelector('[data-new-mode]').value;
        if (mode === 'value') values[field.dataset.newColumn] = field.querySelector('[data-new-value]').value;
        if (mode === 'null') {
          if (field.dataset.newNullable !== 'true') {
            fieldError(
              field.querySelector('[data-new-mode]'),
              language === 'zh-CN' ? '非空字段不能为 NULL。' : 'A NOT NULL column cannot be NULL.',
            );
            return;
          }
          values[field.dataset.newColumn] = null;
        }
      }
    }
    for (const column of schemaInitialColumns({ database: state.database, table: state.table })) {
      if (!column.nullable && Object.hasOwn(values, column.name) && values[column.name] === null) {
        const field = advanced.value.trim()
          ? advanced
          : [...document.querySelectorAll('[data-new-column]')].find(item => item.dataset.newColumn === column.name)
            ?.querySelector('[data-new-mode]');
        fieldError(field, language === 'zh-CN' ? `${column.name} 不能为 NULL。` : `${column.name} cannot be NULL.`);
        return;
      }
    }
    state.newRowDrafts ||= [];
    state.newRowDrafts.push({
      database: state.database,
      table: state.table,
      insertion: true,
      after: values,
      label: `${state.database}.${state.table} · ${language === 'zh-CN' ? '新增行' : 'New row'}`,
    });
    state.overlay = null;
    render();
    return;
  }
  if (action === 'clear-filter') {
    delete state.filters[`${state.database}.${state.table}`];
    delete state.sortColumns[`${state.database}.${state.table}`];
    state.overlay = null;
    render();
    return;
  }
  if (action === 'apply-filter') {
    const columns = Object.keys(databases[state.database][state.table][0] || {});
    const parsed = {};
    for (const [id, kind] of [['filter-advanced-json', 'filters'], ['sort-advanced-json', 'sorts']]) {
      const input = document.getElementById(id);
      if (!input.value.trim()) continue;
      try {
        const entries = JSON.parse(input.value);
        if (!Array.isArray(entries) || entries.some(item => !item || !columns.includes(item.column))) throw new Error();
        if (
          kind === 'filters'
          && entries.some(item =>
            !['eq', 'ne', 'lt', 'lte', 'gt', 'gte', 'like', 'not_like', 'is_null', 'is_not_null'].includes(
              item.op || 'eq',
            ) || (!['is_null', 'is_not_null'].includes(item.op) && !('value' in item))
          )
        ) throw new Error();
        if (
          kind === 'sorts'
          && (entries.some(item => !['asc', 'desc'].includes(item.direction || 'asc'))
            || new Set(entries.map(item => item.column)).size !== entries.length)
        ) throw new Error();
        parsed[kind] = entries;
      } catch {
        fieldError(
          input,
          language === 'zh-CN'
            ? '请输入有效的条件数组、列和操作。'
            : 'Enter a valid array of conditions, columns and operations.',
        );
        return;
      }
    }
    state.filters[`${state.database}.${state.table}`] = parsed.filters
      ?? ($('#filter-column').value
        ? [{ column: $('#filter-column').value, op: $('#filter-operator').value, value: $('#filter-value').value }]
        : []);
    state.sortColumns[`${state.database}.${state.table}`] = parsed.sorts
      ?? ($('#sort-column').value ? [{ column: $('#sort-column').value, direction: $('#sort-direction').value }] : []);
    state.overlay = null;
    state.selected.clear();
    render();
    return;
  }
  if (action === 'draft-discard' || action === 'draft-commit') {
    const drafts = currentDrafts();
    if (state.provider === 'mysql') {
      drafts.forEach(d => {
        if (d.insertion) state.newRowDrafts = state.newRowDrafts.filter(item => item !== d);
        else if (d.deletion) deletedRows.delete(d.row);
        else {
          d.row[d.column] = d.before;
          cellOriginals.get(d.row).delete(d.column);
        }
      });
    } else {
      state.drafts = state.drafts.filter(d => !drafts.includes(d));
      const tab = state.tabs.find(item => item.id === state.active);
      if (tab) tab.redisKeyDraft = null;
    }
    state.overlay = null;
    render();
    if (action === 'draft-commit') {
      toast(
        t('submitDemonstrationCompleteNoDatabaseWriteOccurredAndTheSampleWasReset'),
      );
    }
    return;
  }
  if (action === 'stage-delete' || action === 'stage-delete-entry') {
    const target = state.redisDeleteTarget;
    if (
      !target || target.db !== state.redisDb
      || target.tabId !== state.active || state.connectionModes.redis
    ) return;
    if (target.single) {
      const key = target.keys[0];
      state.drafts.push({
        db: target.db,
        key: key.name,
        kind: 'delete',
        label: `DB ${target.db} / ${key.name} / delete`,
        before: key.name,
        after: t('delete'),
      });
    } else if (target.keys) {
      const tab = state.tabs.find(item => item.id === target.tabId);
      if (!tab || tab.redisKeyDraft) {
        toast(t('closeBlocked'));
        return;
      }
      tab.redisKeyDraft = {
        db: target.db,
        keys: [...target.keys],
        kind: 'delete_keys',
        label: `DB ${target.db} / ${target.keys.length} keys`,
        before: target.keys.map(key => key.name),
        after: t('delete'),
      };
    } else {target.entries.forEach(entry =>
        state.drafts.push({
          db: target.db,
          key: target.key.name,
          kind: 'delete-entry',
          row: entry.index,
          label: `DB ${target.db} / ${target.key.name} / ${entry.index}`,
          before: entry.row,
          after: null,
        })
      );}
    state.redisDeleteTarget = null;
    state.overlay = null;
    render();
    return;
  }
  if (action.startsWith('stage-')) {
    const key = keySamples.find(k => k.name === state.key),
      kind = action.slice(6);
    if (!key) return;
    clearFieldErrors();
    for (
      const input of document.querySelectorAll(
        '[id^="redis-edit-"], #redis-new-name',
      )
    ) {
      if (!input.value.startsWith('base64:')) continue;
      try {
        const encoded = input.value.slice(7);
        if (btoa(atob(encoded)) !== encoded) throw new Error();
      } catch {
        fieldError(
          input,
          language === 'zh-CN'
            ? '请输入有效的 Base64。'
            : 'Enter valid Base64.',
        );
        return;
      }
    }
    let before, after;
    if (kind === 'add-entry') {
      before = null;
      after = [...document.querySelectorAll('[id^="redis-edit-"]')].map(
        input => input.value,
      );
      if (
        key.type === 'ZSET'
          && !/^[+-]?(?:inf|(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?)$/i.test(
            after[1],
          )
        || key.type === 'ZSET' && !/^[+-]?inf$/i.test(after[1])
          && !Number.isFinite(Number(after[1]))
      ) {
        fieldError($('#redis-edit-1'), t('enterAValidScore'));
        return;
      }
      if (
        key.type !== 'LIST' && key.rows.some(row => row[0] === after[0])
      ) {
        fieldError($('#redis-edit-0'), t('enterAnUnusedKeyName'));
        return;
      }
    } else if (kind === 'delete-entry') {
      before = [...key.rows[state.redisRow]];
      after = null;
    } else if (kind === 'edit') {
      before = key.type === 'STR'
        ? [key.value]
        : [...key.rows[state.redisRow]];
      after = before.map((_, i) => $(`#redis-edit-${i}`).value);
      if (
        key.type === 'ZSET'
          && !/^[+-]?(?:inf|(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?)$/i.test(
            after[1],
          )
        || key.type === 'ZSET' && !/^[+-]?inf$/i.test(after[1])
          && !Number.isFinite(Number(after[1]))
      ) {
        fieldError($('#redis-edit-1'), t('enterAValidScore'));
        return;
      }
    } else if (kind === 'ttl') {
      if (
        $('#redis-ttl-policy').value === 'set'
        && !/^\d+$/.test($('#redis-ttl-value').value)
      ) {
        fieldError(
          $('#redis-ttl-value'),
          language === 'zh-CN' ? '请输入非负整数毫秒。' : 'Enter a nonnegative integer number of milliseconds.',
        );
        return;
      }
      before = key.ttl;
      after = $('#redis-ttl-policy').value === 'set'
        ? $('#redis-ttl-value').value + ' ms'
        : $('#redis-ttl-policy').value === 'persist'
        ? '∞'
        : key.ttl;
    } else if (kind === 'rename') {
      before = key.name;
      after = $('#redis-new-name').value;
      if (keySamples.some(k => k.name === after && k !== key)) {
        fieldError($('#redis-new-name'), t('enterAnUnusedKeyName'));
        return;
      }
    } else {
      before = key.name;
      after = t('delete');
    }
    state.drafts = state.drafts.filter(d =>
      !(kind !== 'add-entry' && d.db === state.redisDb
        && d.key === state.key && d.kind === kind
        && (!['edit', 'delete-entry'].includes(kind)
          || d.row === state.redisRow))
    );
    if (JSON.stringify(before) !== JSON.stringify(after)) {
      state.drafts.push({
        db: state.redisDb,
        key: state.key,
        kind,
        row: state.redisRow,
        label: `DB ${state.redisDb} / ${key.name} / ${kind}${kind === 'edit' ? ' / ' + state.redisRow : ''}`,
        before,
        after,
      });
    }
    state.overlay = null;
    render();
    return;
  }
  if (action.startsWith('sql-menu-')) {
    sqlMenuAction(action);
    return;
  }
  if (action === 'profile-create-database:mysql') {
    state.menu = null;
    if (state.connectionStatus.mysql !== 'connected') {
      render();
      toast(az('请先连接数据库', 'Connect first'));
      return;
    }
    if (state.connectionModes.mysql) {
      render();
      toast(az('当前连接为只读', 'The connection is read-only'));
      return;
    }
    state.newDatabaseForm = { name: '', charset: '', collation: '', sql: '' };
    state.overlay = 'new-database';
    render();
    return;
  }
  if (action === 'database-create-execute') {
    const input = document.querySelector('[data-new-database-field="name"]');
    if (!input.reportValidity()) return;
    if (!state.newDatabaseForm.name.trim()) {
      fieldError(input, az('数据库名称不能为空。', 'Database name is required.'));
      return;
    }
    toast(az('执行请求示例 · 未发送', 'Execute request preview · Not sent'));
    return;
  }
  if (action === 'profile-close:mysql') {
    state.menu = null;
    const targets = state.tabs.filter(tab => (tab.provider || 'mysql') === 'mysql');
    if (!closeTabs(targets.map(tab => tab.id))) return;
    state.connectionStatus.mysql = 'disconnected';
    render();
    toast(az('示例连接已关闭，连接配置保留。', 'The example connection was closed; its configuration was retained.'));
    return;
  }
  if (action.startsWith('profile-settings:')) {
    openConnectionForm('connection-settings', action.split(':')[1]);
    return;
  }
  if (action.startsWith('profile-copy:')) {
    copy(action.split(':')[1] === 'mysql' ? 'Commerce' : 'Cache');
    state.menu = null;
    render();
    return;
  }
  if (action.startsWith('profile-delete:')) {
    const provider = action.split(':')[1];
    if (connectionHasDrafts(provider)) {
      state.menu = null;
      render();
      toast(t('resourceHasDrafts'));
      return;
    }
    state.deleteConnectionTarget = {
      provider,
      context: {
        provider: state.provider,
        active: state.active,
        database: state.database,
        table: state.table,
      },
    };
    state.menu = null;
    state.overlay = 'delete-connection';
    render();
    return;
  }
  if (action.startsWith('resource-')) {
    const target = state.menu;
    state.menu = null;
    if (action === 'resource-delete') {
      if (resourceHasDrafts(target.database, target.table)) {
        render();
        toast(t('resourceHasDrafts'));
        return;
      }
      state.deleteResourceTarget = {
        kind: target.kind,
        database: target.database,
        table: target.table,
        resource: target.kind === 'database'
          ? databases[target.database]
          : databases[target.database]?.[target.table],
        context: {
          provider: state.provider,
          active: state.active,
          database: state.database,
          table: state.table,
        },
      };
      state.overlay = 'delete-resource';
      render();
      return;
    }
    if (action === 'resource-copy') copy(target.table ?? target.database);
    else if (action === 'resource-refresh') {
      render();
      toast(t('resourceRefreshPreview'));
      return;
    } else if (target.kind === 'database') {
      state.expanded.has(target.database)
        ? state.expanded.delete(target.database)
        : state.expanded.add(target.database);
    } else {
      openTable(target.database, target.table);
      state.section = action === 'resource-structure'
        ? 'structure'
        : action === 'resource-ddl'
        ? 'ddl'
        : 'data';
    }
    render();
    return;
  }
  if (action.startsWith('sql-statement:')) {
    requestSql('statement', Number(action.split(':')[1]));
    return;
  }
  if (action.startsWith('sql-line:')) {
    state.sqlLine = Number(action.split(':')[1]);
    state.overlay = 'sql-line';
    render();
    return;
  }
  if (action.startsWith('close-tab:')) {
    if (closeTabs([Number(action.split(':')[1])])) render();
    return;
  }
  if (action.startsWith('tabs-') && state.menu?.kind === 'tab') {
    const index = state.tabs.findIndex(tab => tab.id === state.menu.tabId);
    if (index < 0) {
      state.menu = null;
      render();
      return;
    }
    const targets = action === 'tabs-left'
      ? state.tabs.slice(0, index)
      : action === 'tabs-right'
      ? state.tabs.slice(index + 1)
      : state.tabs;
    if (closeTabs(targets.map(tab => tab.id))) render();
    return;
  }
  switch (action) {
    case 'request-preview-import':
    case 'request-import':
    case 'request-export':
    case 'request-file-open':
    case 'request-file-save': {
      const preview = $('#request-preview');
      const path = $('#transfer-path') || $('#query-file-path');
      if (!validateTransfer(path)) return;
      if (
        action === 'request-preview-import' || action === 'request-import'
      ) {
        const signature = JSON.stringify([
          state.provider,
          state.database,
          state.table,
          state.redisDb,
          state.key,
          ...[
            'transfer-path',
            'transfer-format',
            'transfer-mapping',
            'transfer-null',
            'transfer-escape',
            'transfer-binary',
          ].map(id =>
            id === 'transfer-escape' ? String(document.getElementById(id)?.checked) : document.getElementById(id)?.value
          ),
        ]);
        if (
          action === 'request-import' && state.importPreview !== signature
        ) {
          preview.hidden = false;
          preview.textContent = language === 'zh-CN'
            ? '请先使用当前参数预览导入。'
            : 'Preview the import with the current parameters first.';
          return;
        }
        if (action === 'request-preview-import') {
          state.importPreview = signature;
          if (state.overlay === 'import') {
            button.classList.replace('primary', 'button');
            document.querySelector('[data-action="request-import"]')?.classList.replace('button', 'primary');
          }
        }
      }
      preview.hidden = false;
      preview.textContent = `${button.textContent.trim()} · ${path.value} · ${t('requestPreviewOnly')}`;
      break;
    }
    case 'app-settings':
      configPreview.tab = 'general';
      captureLanguageInputs();
      state.overlay = 'app-settings';
      state.menu = null;
      render();
      break;
    case 'shortcut-help':
      state.menu = null;
      state.overlay = 'shortcut-help';
      render();
      break;
    case 'navigation-toggle':
      state.compact = !state.compact;
      if (!state.narrow) state.wideCompact = state.compact;
      render();
      break;
    case 'confirm-delete-resource': {
      const target = state.deleteResourceTarget;
      const resource = target?.kind === 'database'
        ? databases[target.database]
        : databases[target?.database]?.[target?.table];
      const valid = target && state.connections.has('mysql')
        && target.context.provider === 'mysql'
        && resource && resource === target.resource
        && Object.entries(target.context).every(([key, value]) => state[key] === value);
      const hasDrafts = valid
        && resourceHasDrafts(target.database, target.table);
      if (valid && !hasDrafts) {
        if (target.kind === 'database') {
          delete databases[target.database];
          state.visibleDatabases.delete(target.database);
          state.expanded.delete(target.database);
        } else delete databases[target.database][target.table];
        state.tabs = state.tabs.filter(tab =>
          !tab.table || tab.database !== target.database
          || (target.kind === 'resource' && tab.table !== target.table)
        );
        if (!state.tabs.some(tab => tab.id === state.active)) {
          const next = state.tabs[0];
          state.active = next?.id ?? 0;
          if (next) {
            state.database = next.database;
            if (next.table) state.table = next.table;
          }
        }
        state.selected.clear();
        state.anchor = null;
      }
      state.deleteResourceTarget = null;
      state.overlay = null;
      render();
      if (!valid) toast(t('deleteContextChanged'));
      else if (hasDrafts) toast(t('resourceHasDrafts'));
      break;
    }
    case 'select-connection':
      state.compact = false;
      if (!state.narrow) state.wideCompact = false;
      render();
      document.querySelector('.connection.active, .connection')?.focus();
      break;
    case 'new-query': {
      state.provider = 'mysql';
      const tab = {
        id: state.nextTab++,
        title: t('queryFile', { number: state.nextTab }),
        query: true,
        sql: '',
        database: state.database,
      };
      state.tabs.push(tab);
      state.active = tab.id;
      render();
      break;
    }
    case 'new-connection':
      openConnectionForm('new-connection');
      break;
    case 'confirm-delete-connection':
      {
        const target = state.deleteConnectionTarget;
        const valid = target && state.connections.has(target.provider)
          && Object.entries(target.context).every(([key, value]) => state[key] === value);
        const hasDrafts = valid && connectionHasDrafts(target.provider);
        if (valid && !hasDrafts) {
          const targets = state.tabs.filter(tab => (tab.provider || 'mysql') === target.provider);
          if (!closeTabs(targets.map(tab => tab.id))) return;
          state.connectionStatus[target.provider] = 'disconnected';
          state.connections.delete(target.provider);
        }
        state.deleteConnectionTarget = null;
        state.overlay = null;
        render();
        if (!valid) toast(t('deleteContextChanged'));
        else if (hasDrafts) toast(t('resourceHasDrafts'));
      }
      break;
    case 'exit':
      state.overlay = 'exit';
      state.menu = null;
      render();
      break;
    case 'details':
      state.overlay = 'details';
      render();
      break;
    case 'close-modal':
      requestDialogClose(closePreviewDialog);
      break;
    case 'delete-rows': {
      const rows = currentRows().filter(row => state.selected.has(rowId(row)));
      state.menu = null;
      if (!rows.length || rows.some(row => deletedRows.has(row))) {
        render();
        break;
      }
      if (rows.some(row => cellOriginals.get(row)?.size)) {
        render();
        toast(t('deleteRowsEdited'));
        break;
      }
      state.deleteRowsTarget = {
        provider: state.provider,
        active: state.active,
        database: state.database,
        table: state.table,
        rows,
      };
      state.overlay = 'delete-rows';
      render();
      break;
    }
    case 'confirm-delete-rows':
      if (
        !state.deleteRowsTarget
        || ['provider', 'active', 'database', 'table'].some(key => state[key] !== state.deleteRowsTarget[key])
      ) {
        state.deleteRowsTarget = null;
        state.overlay = null;
        render();
        toast(t('deleteContextChanged'));
        break;
      }
      if (
        state.deleteRowsTarget.rows.some(row => cellOriginals.get(row)?.size || deletedRows.has(row))
      ) {
        state.overlay = null;
        render();
        toast(t('deleteRowsEdited'));
        break;
      }
      state.deleteRowsTarget.rows.forEach(row => deletedRows.add(row));
      state.deleteRowsTarget = null;
      state.overlay = null;
      render();
      break;
    case 'copy-insert':
      insert(false);
      break;
    case 'copy-insert-exclude':
      insert(true);
      break;
    case 'copy-sql':
      copy(state.sqlCopy);
      break;
    case 'copy-ddl':
      copy(sampleDdl());
      break;
    case 'sql-all':
      requestSql('all');
      break;
    case 'sql-current':
      requestSql('current');
      break;
    case 'sql-explain':
      requestSql('explain');
      break;
    case 'sql-selection':
      requestSql('selection');
      break;
    case 'sql-close-results': {
      const tab = queryTab();
      tab.execution = null;
      tab.resultFixture = false;
      render();
      break;
    }
    case 'copy-detail':
      copy(
        $('.modal-body details pre')?.textContent
          || $('.modal-body').innerText,
      );
      break;
    case 'copy-rows': {
      const rows = currentRows().filter(row => state.selected.has(rowId(row)));
      copy(
        rows.map(row => Object.values(row).map(v => v === null ? '\\N' : v).join('\t')).join('\n'),
      );
      state.menu = null;
      render();
      break;
    }
    case 'copy-menu-cells': {
      const cell = document.querySelector(
        `#mysql-grid tr[data-row="${CSS.escape(state.menuRow)}"] td[data-cell="${CSS.escape(state.menuCell)}"]`,
      );
      if (!cell) break;
      if (!cell.classList.contains('cell-selected')) {
        const position = gridPosition(cell);
        state.cellSelection = {
          key: `${state.database}.${state.table}`,
          start: position,
          end: position,
        };
      }
      state.selected.clear();
      copyCellSelection(cell);
      state.menu = null;
      render();
      break;
    }
    case 'copy-displayed-text':
      copy(state.menu.displayText);
      state.menu = null;
      render();
      break;
    case 'copy-cells':
      copyCellSelection();
      break;
    case 'inspect-cell':
      state.menu = null;
      state.cellDrawer = true;
      state.cellInspectorValue = null;
      state.cellInspectorDirty = false;
      state.cellOverlayBaseline = null;
      state.cellInspectorIsNull = databases[state.database][state.table].find(row =>
        rowId(row) === state.menuRow
      )?.[state.menuCell] === null;
      render();
      document.querySelector('.cell-drawer #cell-value')?.focus();
      break;
    case 'close-cell-drawer':
      requestDialogClose(() => {
        state.cellDrawer = false;
        state.cellInspectorValue = null;
        render();
      });
      break;
    case 'cell-open-overlay':
      state.cellOverlayBaseline = {
        column: state.menuCell,
        value: state.cellInspectorValue,
        isNull: state.cellInspectorIsNull,
        dirty: state.cellInspectorDirty,
      };
      state.overlay = 'cell-details';
      render();
      break;
    case 'edit-cell':
      startCellEdit(state.menuRow, state.menuCell);
      break;
    case 'cell-advanced': {
      const row = databases[state.database][state.table].find(item => rowId(item) === state.menuRow);
      state.cellValueTarget = {
        database: state.database,
        table: state.table,
        row: state.menuRow,
        column: state.menuCell,
        value: state.cellInspectorIsNull ? null : $('#cell-value').value,
        readonly: Boolean(deletedRows.has(row) || state.connectionModes.mysql),
      };
      state.valuePreviewTarget = null;
      state.cellAdvancedReturn = state.overlay === 'cell-details' ? 'cell-details' : null;
      state.overlay = 'advanced-value';
      render();
      break;
    }
    case 'cell-json':
      try {
        JSON.parse($('#cell-value').value);
      } catch {
        fieldError($('#cell-value'), language === 'zh-CN' ? '请输入有效 JSON。' : 'Enter valid JSON.');
        break;
      }
      button.closest('.modal, .cell-drawer').querySelector('[data-action="cell-stage"]')?.click();
      break;
    case 'cell-stage':
    case 'cell-null': {
      const row = databases[state.database][state.table].find(row => rowId(row) === state.menuRow);
      if (
        row && !deletedRows.has(row) && !state.connectionModes.mysql
        && (action !== 'cell-null' || state.menuCell === 'email')
      ) {
        stageCellValue(
          row,
          state.menuCell,
          action === 'cell-null' || state.cellInspectorIsNull ? null : $('#cell-value').value,
        );
      }
      state.overlay = null;
      state.cellInspectorValue = null;
      state.cellInspectorIsNull = false;
      state.cellOverlayBaseline = null;
      state.cellInspectorDirty = false;
      dialogDraftDirty = false;
      render();
      break;
    }
    case 'refresh-keys':
      state.selectedKeys.clear();
      state.keyAnchor = null;
      render();
      break;
    case 'key-list':
    case 'key-tree':
      state.keyTree = action === 'key-tree';
      render();
      break;
    case 'copy-key':
      copy(state.key);
      break;
    case 'copy-value': {
      const key = keySamples.find(k => k.name === state.key);
      if (key) copy(redisClipboardValue(key));
      break;
    }
    case 'cli-toggle':
      state.cli = !state.cli;
      render();
      break;
    case 'key-browser':
      state.cli = false;
      render();
      break;
    case 'cli-history':
      state.cliHistory = !state.cliHistory;
      render();
      break;
    case 'cli-run':
      if (!state.cliText.trim()) break;
      appendCliRecord(state.cliEntries, {
        command: state.cliText,
        database: String(state.cliDb ?? az('未知', 'Unknown')),
        instance: 'Cache',
        status: 'success',
        ms: 1,
        text: t('cliPreview'),
      });
      // 仅模拟单条 SELECT 的目标变化，不连接数据库。
      if (/^\s*SELECT\s+\d+\s*$/i.test(state.cliText)) {
        state.cliDb = Number(state.cliText.trim().split(/\s+/)[1]);
      }
      state.cliState = 'success';
      render();
      break;
    case 'cli-cancel': {
      const pending = state.cliEntries.findLast(entry => entry.status === 'running');
      if (pending) {
        pending.status = 'unknown';
        pending.text = az(
          '已结束等待；远端状态未知。',
          'Stopped waiting; remote state is unknown.',
        );
      }
      state.cliState = 'success';
      render();
      break;
    }
    case 'load-databases':
      if (!validateConnection()) return;
      if (
        state.overlay === 'new-connection' && !state.connectionForm.dbLoaded
      ) {
        state.connectionForm.selectedDatabases = new Set(
          Object.keys(databases),
        );
      }
      state.connectionForm.dbLoaded = true;
      document.querySelector('.database-scope').outerHTML = databaseScope();
      break;
    case 'db-select-matches':
      matchingDatabases().forEach(name => state.connectionForm.selectedDatabases.add(name));
      syncDatabaseOptions();
      break;
    case 'db-clear-matches':
      matchingDatabases().forEach(name => state.connectionForm.selectedDatabases.delete(name));
      syncDatabaseOptions();
      break;
    case 'demo-save':
      if (!validateConnection()) return;
      if (state.connectionForm.provider === 'redis') {
        const override = document.querySelector('[data-config-redis-override]');
        if (override && !override.reportValidity()) return;
      }
      if (state.overlay === 'connection-settings') {
        if (state.connectionForm.provider === 'redis') {
          state.redisDatabasesOverride = state.connectionForm.redisDatabasesOverride;
        }
        if (state.connectionForm.provider === 'mysql') {
          state.mysqlOptions = {
            timezone: state.connectionForm.mysqlTimezone,
            connectTimeout: state.connectionForm.mysqlConnectTimeout,
            queryTimeout: state.connectionForm.mysqlQueryTimeout,
          };
        }
        state.connectionModes[state.connectionForm.provider] = state.connectionForm.readOnly;
        state.connectionStatus[state.connectionForm.provider] = 'connected';
        state.overlay = null;
        render();
      }
      toast(t('connectionPreview'));
      break;
    case 'demo-test':
      if (!validateConnection()) return;
      toast(t('connectionPreview'));
      break;
    case 'demo-exit':
      state.overlay = null;
      render();
      toast(t('exitPreview'));
      break;
    default:
      toast(button.title || t('actionPreview'));
  }
});

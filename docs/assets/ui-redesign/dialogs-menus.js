'use strict';
function fact(label, value) {
  return `<div class="fact"><dt>${h(label)}</dt><dd>${h(value)}</dd></div>`;
}
function detail(kind) {
  let title, body;
  const summary = (label, status, severity = 'good') =>
    `<div class="detail-summary">${icon('info')}<strong>${
      h(label)
    }</strong><span class="spacer"></span><span class="tag ${severity}">${h(status)}</span></div>`;
  if (kind === 'error') {
    title = t('executionDetails');
    body = summary(t('sqlFailed'), t('failure'), 'danger')
      + `<dl class="facts">${fact(t('connectionDatabase'), 'Commerce / commerce')}${fact(t('duration'), '3 ms')}${
        fact(t('errorCode'), '1146 · SQLSTATE 42S02')
      }${
        fact(t('operation'), t('executeSql'))
      }</dl><div class="notice error-notice">Table 'commerce.missing_table' doesn't exist</div><section class="detail-section"><h3>${
        t('executedStatement')
      }</h3><pre class="value" style="padding:0">SELECT * FROM missing_table;</pre></section>`;
  } else if (kind === 'draft' || kind === 'restore') {
    title = t(kind === 'restore' ? 'restoreDetails' : 'draftDetails');
    body = summary(
      t(kind === 'restore' ? 'restoredIntent' : 'pendingChanges'),
      t('notWritten'),
      'warn',
    )
      + `<dl class="facts">${fact(t('connection'), 'Commerce')}${fact(t('resource'), 'commerce.orders')}${
        fact(t('selectedPrimaryKey'), 'id = 10421')
      }${fact(t('changeCount'), '1')}</dl>${
        kind === 'restore'
          ? `<div class="notice">${t('draftRestored')}</div>`
          : ''
      }<section class="detail-section"><h3>${t('fieldChanges')}</h3>${
        simpleTable([t('field'), t('oldValue'), t('newValue')], [[
          'status',
          'pending',
          'paid',
        ]])
      }</section>`;
  } else if (kind === 'import') {
    title = t('importDetails');
    body = summary('orders.csv', t('previewComplete'))
      + `<dl class="facts">${fact(t('connection'), 'Commerce')}${fact(t('target'), 'commerce.orders')}${
        fact(t('format'), 'CSV · UTF-8')
      }${fact(t('previewRows'), '3')}${
        fact(t('pendingErrors'), t('convertError'))
      }</dl><section class="detail-section"><h3>${t('columnMapping')}</h3>${
        simpleTable([t('fileColumn'), t('targetField'), t('conversion')], [[
          'order_no',
          'order_no',
          t('text'),
        ], ['total', 'total', 'DECIMAL(12,2)']])
      }</section><section class="detail-section"><h3>${t('rowErrors')}</h3>${
        simpleTable([
          t('rowNumber'),
          t('field'),
          t('oldValue'),
          t('reason'),
        ], [['3', 'total', 'unknown', t('invalidDecimal')]])
      }</section>`;
  } else if (kind === 'schema') {
    title = t('resourceDetails');
    body = summary('commerce.orders', t('baseTable'))
      + `<dl class="facts">${fact(t('connection'), 'Commerce')}${fact(t('engine'), 'InnoDB')}${
        fact(t('primaryKey'), 'id')
      }${fact(t('editability'), t('reliableKey'))}</dl><section class="detail-section"><h3>${t('field')}</h3>${
        simpleTable([t('field'), t('type'), 'NULL', t('constraint')], [
          ['id', 'BIGINT', t('no'), t('primaryAuto')],
          ['status', 'VARCHAR(32)', t('no'), '—'],
          ['total', 'DECIMAL(12,2)', t('no'), '—'],
        ])
      }</section>`;
  } else {
    title = t('taskDetails');
    body = summary(t('browseTable'), t('completed'))
      + `<dl class="facts">${fact(t('connectionDatabase'), 'Commerce / commerce')}${
        fact(t('resource'), 'commerce.orders')
      }${
        fact(
          t('duration'),
          language === 'zh-CN' ? '未记录' : 'Not recorded',
        )
      }${fact(t('result'), t('dimensions'))}${
        fact(t('started'), language === 'zh-CN' ? '未记录' : 'Not recorded')
      }</dl><section class="detail-section"><h3>${t('resultSet')}</h3>${
        simpleTable([t('result'), t('type'), t('rowCount'), t('status')], [[
          t('resultOne'),
          t('tableData'),
          language === 'zh-CN' ? '未记录' : 'Not recorded',
          t('readComplete'),
        ]])
      }</section><section class="detail-section"><h3>${t('executedStatement')}</h3><span class="muted">${
        language === 'zh-CN' ? '未记录' : 'Not recorded'
      }</span></section>`;
  }
  const diagnosticText = new DOMParser().parseFromString(body, 'text/html')
    .body.textContent.trim();
  const diagnostic = JSON.stringify(
    {
      kind,
      sample: true,
      ...(kind === 'task' ? { taskId: '7e91c151-13bc-4823-a197-1efb6f27c4a2' } : {}),
      content: diagnosticText,
    },
    null,
    2,
  );
  body += `<details class="detail-section"><summary>${h(t('fullDiagnostics'))}</summary><pre class="preview-code">${
    h(diagnostic)
  }</pre></details>`;
  return modal(
    title,
    body,
    `<span class="spacer"></span><button class="button" data-action="close-modal">${
      h(t('close'))
    }</button><button class="button row" data-action="copy-detail">${icon('copy')} ${t('copyDetails')}</button>`,
  );
}
function modal(title, body, foot = '', className = '') {
  requestAnimationFrame(syncConditionalRequirements);
  return `<div class="overlay"><section class="modal ${h(className)}" role="dialog" aria-modal="true" aria-label="${
    h(title)
  }"><header class="modal-head"><h2>${h(title)}</h2><span class="spacer"></span>${
    ib('x', t('closeDialog'), 'close-modal')
  }</header><div class="modal-body">${body}</div><footer class="modal-foot">${
    foot
    || `<span class="spacer"></span><button class="button" data-action="close-modal">${h(t('close'))}</button>`
  }</footer></section></div>`;
}
function cellInspector(drawer) {
  const row = databases[state.database]?.[state.table]?.find(item => rowId(item) === state.menuRow);
  if (!row) return '';
  const value = state.cellInspectorIsNull
    ? ''
    : state.cellInspectorValue ?? (row[state.menuCell] === null
      ? ''
      : typeof row[state.menuCell] === 'object'
      ? JSON.stringify(row[state.menuCell], null, 2)
      : String(row[state.menuCell]));
  const blocked = deletedRows.has(row)
    ? (language === 'zh-CN' ? '该行已标记删除。' : 'This row is marked for deletion.')
    : state.connectionModes.mysql
    ? (language === 'zh-CN' ? '连接为只读。' : 'The connection is read-only.')
    : '';
  const body = `<dl class="facts">${fact(t('resource'), state.database + '.' + state.table)}${
    fact(t('rowField'), 'id = ' + state.menuRow)
  }</dl><label>${h(t('field'))}<select id="cell-field">${
    Object.keys(row).map(column =>
      `<option value="${h(column)}" ${column === state.menuCell ? 'selected' : ''}>${h(column)}</option>`
    ).join('')
  }</select></label><label>${h(t('value'))}${
    state.cellInspectorIsNull ? ' <span class="null">NULL</span>' : ''
  }<textarea id="cell-value" rows="8" ${blocked ? 'readonly' : ''}>${h(value)}</textarea></label>${
    blocked ? `<p class="notice">${h(blocked)}</p>` : ''
  }<button type="button" data-action="cell-advanced">${
    language === 'zh-CN' ? '高级值编辑' : 'Advanced value editor'
  }</button>${
    drawer
      ? `<button type="button" data-action="cell-open-overlay">${
        language === 'zh-CN' ? '在完整值窗口中打开' : 'Open full value window'
      }</button>`
      : ''
  }`;
  const foot = `<button data-action="cell-null" ${blocked || state.menuCell !== 'email' ? 'disabled' : ''}>${
    h(t('setNull'))
  }</button><button data-action="cell-json" ${blocked ? 'disabled' : ''}>${
    language === 'zh-CN' ? '设置 JSON' : 'Set JSON'
  }</button><span class="spacer"></span><button data-action="${drawer ? 'close-cell-drawer' : 'close-modal'}">${
    h(t('close'))
  }</button><button class="primary" data-action="cell-stage" ${blocked ? 'disabled' : ''}>${
    h(t('stageDraft'))
  }</button>`;
  if (!drawer) return modal(t('cellDetails'), body, foot);
  return `<aside class="cell-drawer" aria-label="${h(t('cellDetails'))}"><header class="modal-head"><h2>${
    h(t('cellDetails'))
  }</h2><span class="spacer"></span>${
    ib('x', t('closeDialog'), 'close-cell-drawer')
  }</header><div class="modal-body">${body}</div><footer class="modal-foot">${foot}</footer></aside>`;
}
function overlay() {
  if (!state.overlay) return '';
  if (serverForm || serverDelete || connectionDelete || authStage) {
    return '';
  }
  if (state.overlay.startsWith('sample-')) return samplePreview(state.overlay);
  if (state.overlay === 'view-resource-preview') return viewResourcePreview();
  if (state.overlay === 'replace-design') return replacementPreview();
  if (state.overlay === 'advanced-value') return advancedValuePreview();
  if (state.overlay === 'columns') {
    const key = state.columnsTarget;
    const redisKey = key.startsWith('redis:')
      ? keySamples.find(item => item.name === state.key)
      : null;
    const names = redisKey
      ? (redisKey.type === 'HASH'
        ? [t('field'), t('value')]
        : redisKey.type === 'LIST'
        ? [t('index'), t('value')]
        : redisKey.type === 'ZSET'
        ? [t('member'), t('score')]
        : [t('member')])
      : key === 'result'
      ? ['id', 'order_no', 'status', 'total']
      : Object.keys(databases[state.database]?.[state.table]?.[0] || {});
    return modal(
      t('columnSettings'),
      `<div class="fields">${
        names.map((name, i) => {
          const id = redisKey ? String(i) : name;
          return `<div><label><input type="checkbox" data-column-choice="${h(id)}" ${
            state.hiddenColumns[key]?.includes(id) ? '' : 'checked'
          }> ${h(name)}</label>
            <label>${
            language === 'zh-CN' ? '宽度（像素，留空自动）' : 'Width (pixels, blank for auto)'
          } <input type="number" min="1" max="65535" data-column-width="${h(id)}" value="${
            state.columnWidths[key]?.[id] ?? ''
          }"></label>
            <label><input type="checkbox" data-column-freeze="${h(id)}" ${
            state.frozenColumns?.[key]?.includes(id) ? 'checked' : ''
          }> ${language === 'zh-CN' ? '冻结列' : 'Freeze column'} · ${h(name)}</label></div>`;
        }).join('')
      }</div>`,
      `<span class="spacer"></span><button data-action="apply-columns" class="primary">${h(t('applyFilter'))}</button>`,
    );
  }
  if (
    [
      'draft-preview',
      'filter',
      'new-row',
      'export',
      'import',
      'add-entry',
      'delete-entry',
      'transfer',
      'query-file',
      'ttl',
      'rename-key',
      'delete-key',
      'edit-key',
    ].includes(state.overlay)
  ) return formOverlay(state.overlay);
  if (state.overlay === 'history') {
    const hasHistory = historySamples.some(item =>
      `${item.sql} ${item.time} ${item.status} ${item.status === 'success' ? t('successSample') : t('errorSample')}`
        .toLowerCase().includes((state.historyApplied ?? '').toLowerCase())
    );
    const historyBlocked = !hasHistory
      ? (language === 'zh-CN' ? '请先选择历史记录。' : 'Select a history entry first.')
      : currentDrafts().length
      ? (language === 'zh-CN' ? '请先处理当前草稿。' : 'Resolve the current draft first.')
      : queryTab()?.running
      ? (language === 'zh-CN' ? '任务正在运行。' : 'A task is running.')
      : '';
    return modal(
      t('queryHistory'),
      `<div class="history-layout">${historyPanel()}</div>`,
      `<span class="muted small">${
        h(historyBlocked)
      }</span><span class="spacer"></span><button data-action="close-modal">${
        h(t('close'))
      }</button><button class="primary" data-action="open-history" ${historyBlocked ? 'disabled' : ''}>${
        h(t('openInQuery'))
      }</button>`,
    );
  }
  if (state.overlay === 'delete-rows') {
    const target = state.deleteRowsTarget;
    return modal(
      t('deleteRowsConfirm'),
      `<p>${h(target.database)}.${h(target.table)} · ${h(t('selectedRows', { count: target.rows.length }))}</p><ul>${
        target.rows.map(row => `<li><code>${h(rowId(row))}</code></li>`).join('')
      }</ul><p class="muted">${h(t('deleteRowsHelp'))}</p>`,
      `<span class="spacer"></span><button class="button" data-action="close-modal">${
        t('cancel')
      }</button><button class="button danger" data-action="confirm-delete-rows">${t('stageDraft')}</button>`,
    );
  }
  if (state.overlay === 'delete-connection') {
    const provider = state.deleteConnectionTarget.provider;
    return modal(
      t('deleteConnection'),
      `<p>${h(provider === 'mysql' ? 'Commerce · MySQL' : 'Cache · Redis')}</p><p class="muted">${
        h(t('deleteConnectionHelp'))
      }</p>${deleteStateExamples()}`,
      `<span class="spacer"></span><button class="button" data-action="close-modal">${
        t('cancel')
      }</button><button class="button danger" data-action="confirm-delete-connection">${
        t('deleteConnection')
      }</button>`,
    );
  }
  if (state.overlay === 'delete-resource') {
    const target = state.deleteResourceTarget;
    const isTable = target.kind === 'resource';
    return modal(
      t(isTable ? 'deleteTable' : 'deleteDatabase'),
      `<p>${h(target.database)}${isTable ? '.' + h(target.table) : ''}</p><p class="muted">${
        h(t(isTable ? 'deleteTableHelp' : 'deleteDatabaseHelp'))
      }</p>${deleteStateExamples()}`,
      `<span class="spacer"></span><button class="button" data-action="close-modal">${
        t('cancel')
      }</button><button class="button danger" data-action="confirm-delete-resource">${
        t(isTable ? 'deleteTable' : 'deleteDatabase')
      }</button>`,
    );
  }
  if (state.overlay === 'shortcut-help') {
    const mod = isMac ? 'Command' : 'Ctrl';
    const groups = ['Global', 'Sql', 'Text', 'Grid', 'Inline', 'Cli'];
    return modal(
      t('shortcutHelp'),
      `<p class="muted small">${h(t('shortcutPreviewNote'))}</p>${
        groups.map(group =>
          `<section class="detail-section"><h3>${h(t('shortcut' + group))}</h3><p>${
            h(t('shortcut' + group + 'Keys', { mod }))
          }</p></section>`
        ).join('')
      }`,
    );
  }
  if (state.overlay === 'app-settings') {
    return modal(
      t('settings'),
      settingsPreview(),
      settingsPreviewFooter(),
      'settings-modal',
    );
  }
  if (state.overlay === 'cell-details') return cellInspector(false);
  if (state.overlay === 'details') return detail(state.detailKind);
  if (state.overlay === 'sql-copy') {
    return modal(
      t(state.sqlCopyExclude ? 'copyInsertExclude' : 'copyInsert'),
      `<div class="detail-summary"><span>${
        t('selectedRows', { count: state.selected.size })
      }</span><span class="spacer"></span><span class="tag">${h(state.database)}.${
        h(state.table)
      }</span></div><textarea class="sql-preview" aria-label="${t('insertPreview')}" readonly>${
        h(state.sqlCopy)
      }</textarea><p class="muted small">${t(state.sqlCopyExclude ? 'insertHelpExclude' : 'insertHelp')}</p>`,
      `<span class="spacer"></span><button class="button" data-action="close-modal">${
        t('cancel')
      }</button><button class="primary row" data-action="copy-sql">${icon('copy')} ${t('copy')}</button>`,
    );
  }
  if (state.overlay === 'new-database') return newDatabaseDialog();
  if (
    state.overlay === 'new-connection'
    || state.overlay === 'connection-settings'
  ) {
    return modal(
      state.overlay === 'new-connection'
        ? t('newConnection')
        : t('connectionSettings'),
      connectionFormBody(),
      `<span class="muted small">${
        t(
          state.overlay === 'connection-settings'
            ? 'connectionEditExample'
            : 'connectionFormExample',
        )
      }</span><span class="spacer"></span><button class="button" data-action="demo-test">${
        t('testConnection')
      }</button><button class="primary" data-action="demo-save">${
        t(
          state.overlay === 'connection-settings'
            ? 'saveAndReconnect'
            : 'saveConnection',
        )
      }</button>`,
    );
  }
  if (state.overlay === 'sql-line') {
    const tab = queryTab(), parsed = scanSql(tab.sql);
    return modal(
      t('sqlOnLine', { line: state.sqlLine }),
      `<div class="muted small">${t('chooseStatement')}</div>${
        parsed.statements.map((statement, index) =>
          statement.line === state.sqlLine
            ? `<section class="detail-section"><div class="row"><strong>SQL ${
              index + 1
            }</strong><span class="spacer"></span><button class="button row" data-action="sql-statement:${index}">${
              icon('play')
            } ${t('executeStatement')}</button></div><pre class="sql-execution-text">${
              highlightSql(tab.sql.slice(statement.start, statement.end))
            }</pre></section>`
            : ''
        ).join('')
      }`,
    );
  }
  if (state.overlay === 'exit') {
    return modal(
      t('exitTitle'),
      `<p>${t('exitHelp')}</p><p class="muted small">${t('exitShortcut')}</p>`,
      `<span class="muted small">${
        t('designPreview')
      }</span><span class="spacer"></span><button class="button" data-action="close-modal">${
        t('back')
      }</button><button class="primary" data-action="demo-exit">${t('saveExit')}</button>`,
    );
  }
  return '';
}
function menu() {
  if (!state.menu) return '';
  if (
    state.menu.kind === 'redis-key' || state.menu.kind === 'redis-entry'
  ) {
    const keyMenu = state.menu.kind === 'redis-key';
    const count = keyMenu
      ? state.selectedKeys.size
      : state.selectedEntries.size;
    const zh = language === 'zh-CN';
    return `<div class="context-menu" role="menu" style="left:${state.menu.x}px;top:${state.menu.y}px"><div class="menu-caption">${
      h(state.menu.key.name)
    } · ${count}</div><button role="menuitem" data-action="redis-menu-open">${icon('eye')} ${
      keyMenu
        ? (zh ? '打开目标 key' : 'Open target key')
        : (zh ? '打开当前 key' : 'Open current key')
    }</button><button role="menuitem" data-action="redis-menu-copy">${icon('copy')} ${
      keyMenu
        ? (zh ? '复制选中 key 名称' : 'Copy selected key names')
        : (zh ? '复制选中条目' : 'Copy selected entries')
    }</button><button class="danger" role="menuitem" data-action="redis-menu-delete" ${
      state.connectionModes.redis ? 'disabled' : ''
    }>${icon('trash')} ${
      keyMenu
        ? (zh ? '删除选中 key' : 'Delete selected keys')
        : (zh ? '删除选中条目' : 'Delete selected entries')
    }</button></div>`;
  }
  if (state.menu.kind === 'tab') {
    const index = state.tabs.findIndex(tab => tab.id === state.menu.tabId);
    if (index < 0) return '';
    return `<div class="context-menu" role="menu" style="left:${state.menu.x}px;top:${state.menu.y}px"><div class="menu-caption">${
      h(state.tabs[index].title)
    }</div><button role="menuitem" data-action="tabs-left" ${index === 0 ? 'disabled' : ''}>${icon('arrow-left')} ${
      h(t('closeLeft'))
    }</button><button role="menuitem" data-action="tabs-right" ${index === state.tabs.length - 1 ? 'disabled' : ''}>${
      icon('arrow-right')
    } ${h(t('closeRight'))}</button><hr><button role="menuitem" data-action="tabs-all">${icon('x')} ${
      h(t('closeAll'))
    }</button></div>`;
  }
  if (state.menu.kind === 'sql') {
    const selected = queryTab()?.sql.slice(state.menu.start, state.menu.end)
      || '';
    return `<div class="context-menu" role="menu" aria-label="${
      t('sqlMenu')
    }" style="left:${state.menu.x}px;top:${state.menu.y}px"><div class="menu-caption">${
      t('sqlSelection')
    }</div><button role="menuitem" data-action="sql-menu-execute">${icon('play')} ${
      t('executeSelectedSql')
    }</button><button role="menuitem" data-action="sql-menu-explain" ${
      explainableSelection(selected)
        ? ''
        : `disabled title="${t('explainLimit')}"`
    }>${icon('chart')} ${t('explainPlan')}</button><button role="menuitem" data-action="sql-menu-format">${
      icon('wand')
    } ${t('formatSelection')}</button><hr><button role="menuitem" data-action="sql-menu-copy">${icon('copy')} ${
      t('copy')
    }</button><button role="menuitem" data-action="sql-menu-cut">${icon('scissors')} ${
      t('cut')
    }</button><button role="menuitem" data-action="sql-menu-all">${icon('selection')} ${t('selectAll')}</button></div>`;
  }
  if (state.menu.kind === 'connection') {
    const provider = state.menu.provider,
      aid = state.menu.archeryId,
      sid = state.menu.archeryServer || activeServerId;
    const c = aid
      && archeryConnections.find(item => item.server === sid && item.instance === aid);
    const attrs = (verb) =>
      aid
        ? `data-archery="connection-${verb}:${aid}" data-connection-server="${h(sid)}"`
        : `data-action="profile-${verb === 'edit' ? 'settings' : verb}:${provider}"`;
    return `<div class="context-menu" role="menu" aria-label="${
      t('connectionMenu')
    }" style="left:${state.menu.x}px;top:${state.menu.y}px"><div class="menu-caption">${
      h(
        c?.name
          || (provider === 'mysql' ? 'Commerce · MySQL' : 'Cache · Redis'),
      )
    }</div>${
      !aid && provider === 'mysql'
        ? `<button role="menuitem" data-action="profile-create-database:mysql" ${
          state.connectionModes.mysql || state.connectionStatus.mysql !== 'connected' ? 'disabled' : ''
        } title="${
          state.connectionStatus.mysql !== 'connected'
            ? h(az('请先连接数据库', 'Connect first'))
            : state.connectionModes.mysql
            ? h(az('当前连接为只读', 'The connection is read-only'))
            : ''
        }">${icon('plus')} ${
          az('新建数据库…', 'Create database…')
        }</button><button role="menuitem" data-action="profile-close:mysql">${icon('x')} ${
          az('关闭连接', 'Close connection')
        }</button>`
        : ''
    }<button role="menuitem" ${attrs('edit')}>${icon('gear')} ${
      t('connectionSettings')
    }</button><button role="menuitem" ${attrs('copy')}>${icon('copy')} ${
      t('copyConnectionName')
    }</button><button class="danger" role="menuitem" ${attrs('delete')}>${icon('trash')} ${
      t('deleteConnection')
    }</button></div>`;
  }
  if (state.menu.kind === 'database' || state.menu.kind === 'resource') {
    const item = state.menu;
    const table = item.kind === 'resource';
    const readOnly = Boolean(state.connectionModes.mysql);
    const readOnlyReason = language === 'zh-CN'
      ? '只读连接不能新建或删除资源。'
      : 'Resources cannot be created or deleted on a read-only connection.';
    return `<div class="context-menu" role="menu" aria-label="${
      h(t('tableMenu'))
    }" style="left:${item.x}px;top:${item.y}px"><div class="menu-caption">${
      h(table ? item.table : item.database)
    }</div><button role="menuitem" data-action="resource-open">${icon('eye')} ${
      h(
        table
          ? t('openResource')
          : state.expanded.has(item.database)
          ? (language === 'zh-CN' ? '折叠数据库' : 'Collapse database')
          : t('expandDatabase'),
      )
    }</button><button role="menuitem" data-action="resource-refresh">${icon('repeat')} ${
      h(t(table ? 'refreshData' : 'refreshResources'))
    }</button>${
      table
        ? `<button role="menuitem" data-action="resource-structure">${icon('columns')} ${
          h(t('structure'))
        }</button><button role="menuitem" data-action="resource-ddl">${
          icon('terminal')
        } DDL</button><button role="menuitem" data-action="schema-trigger-list">${
          language === 'zh-CN' ? '触发器列表' : 'Trigger list'
        }</button>`
        : `<button role="menuitem" data-action="schema-create:new-table" ${readOnly ? 'disabled' : ''}>${
          language === 'zh-CN' ? '新建表' : 'New table'
        }</button><button role="menuitem" data-action="schema-create:view" ${readOnly ? 'disabled' : ''}>${
          language === 'zh-CN' ? '新建视图' : 'New view'
        }</button><button role="menuitem" data-action="schema-create:trigger" ${readOnly ? 'disabled' : ''}>${
          language === 'zh-CN' ? '新建触发器' : 'New trigger'
        }</button>`
    }<button role="menuitem" data-action="resource-copy">${icon('copy')} ${
      h(t('copyResourceName'))
    }</button><button class="danger" role="menuitem" data-action="resource-delete" ${readOnly ? 'disabled' : ''}>${
      icon('trash')
    } ${h(t(table ? 'deleteTable' : 'deleteDatabase'))}</button>${
      readOnly ? `<div class="menu-caption">${h(readOnlyReason)}</div>` : ''
    }</div>`;
  }
  const selectedRows = currentRows().filter(row => state.selected.has(rowId(row)));
  return `<div class="context-menu" role="menu" aria-label="${
    t('tableMenu')
  }" style="left:${state.menu.x}px;top:${state.menu.y}px"><div class="menu-caption">${
    t('selectedRows', { count: state.selected.size })
  }</div>${
    state.menu.cell
      ? `<button role="menuitem" data-action="copy-menu-cells">${icon('copy')} ${t('copySelectedCells')}</button>`
      : ''
  }<button role="menuitem" data-action="copy-rows">${icon('copy')} ${
    t('copyRows')
  }</button><button role="menuitem" data-action="copy-insert">${icon('copy')} ${
    t('copyInsert')
  }</button><button role="menuitem" data-action="copy-insert-exclude">${icon('copy')} ${
    t('copyInsertExclude')
  }</button><button class="danger" role="menuitem" data-action="delete-rows" ${
    !selectedRows.length || selectedRows.some(row => deletedRows.has(row))
      ? 'disabled'
      : ''
  }>${icon('trash')} ${
    t('deleteRows', { count: state.selected.size })
  }</button><hr role="separator"><button role="menuitem" data-action="copy-displayed-text">${icon('copy')} ${
    t('copyDisplayedText')
  }</button></div>`;
}
function closeTabs(ids) {
  const targets = state.tabs.filter(tab => ids.includes(tab.id));
  const blocked = targets.some(tab =>
    tab.running || ((tab.provider || 'mysql') === 'mysql'
      ? ((!tab.schemaCreateKind && !tab.query && resourceHasDrafts(tab.database, tab.table))
        || (!tab.query && schemaHasDraft(tab)))
      : tab.redisKeyDraft || state.drafts.some(d => d.db === tab.redisDb && d.key === tab.key)
        || state.cliState === 'running')
  );
  if (blocked) {
    state.menu = null;
    render();
    toast(t('closeBlocked'));
    return false;
  }
  const oldIndex = state.tabs.findIndex(tab => tab.id === state.active);
  state.tabs = state.tabs.filter(tab => !ids.includes(tab.id));
  if (!state.tabs.some(tab => tab.id === state.active)) {
    const next = state
      .tabs[Math.min(Math.max(0, oldIndex - 1), state.tabs.length - 1)];
    state.active = next?.id ?? 0;
    if (next) {
      state.provider = next.provider || 'mysql';
      state.database = next.database || state.database;
      state.table = next.table || state.table;
      if (next.key) {
        state.key = next.key;
        state.redisDb = next.redisDb;
      }
    }
  }
  state.selected.clear();
  state.anchor = null;
  state.menu = null;
  return true;
}
function activateProvider(provider) {
  state.provider = provider;
  let tab = state.tabs.find(item => (item.provider || 'mysql') === provider);
  if (!tab && provider === 'redis') {
    tab = {
      id: state.nextTab++,
      provider: 'redis',
      title: state.key,
      key: state.key,
      redisDb: state.redisDb,
    };
    state.tabs.push(tab);
  } else if (!tab && provider === 'mysql') {
    tab = {
      id: state.nextTab++,
      provider: 'mysql',
      query: true,
      title: t('queryFile', { number: state.nextTab }),
      sql: '',
      database: state.database,
    };
    state.tabs.push(tab);
  }
  state.active = tab?.id ?? 0;
  if (tab?.database) state.database = tab.database;
  if (tab?.table) state.table = tab.table;
  if (tab?.key) {
    state.key = tab.key;
    state.redisDb = tab.redisDb;
  }
}

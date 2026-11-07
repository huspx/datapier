'use strict';
function mysql() {
  const tab = state.tabs.find(t => t.id === state.active);
  if (!tab) {
    return `${tabs()}<div class="empty empty-workspace"><strong>${
      h(t('emptyWorkspace'))
    }</strong><div class="empty-actions"><button data-action="new-query">${h(t('newQuery'))}</button>${
      state.compact
        ? `<button data-action="select-connection">${h(t('selectConnection'))}</button>`
        : ''
    }</div></div>`;
  }
  if (tab.query) return `${tabs()}${queryPanel(tab)}`;
  const pageId = `mysql:${state.database}.${state.table}`;
  const pageState = resultPageState(pageId);
  const countCriteria = JSON.stringify(state.filters[`${state.database}.${state.table}`] || null);
  if (pageState.criteria !== countCriteria) {
    pageState.criteria = countCriteria;
    pageState.counted = false;
    pageState.page = 0;
  }
  const matching = new Set(currentRows().map(rowId));
  const savedFilter = state.rowFilter;
  state.rowFilter = '';
  const rows = currentRows();
  const gridMarkup = state.section === 'data' ? grid() : '';
  state.rowFilter = savedFilter;
  const pageRows = resultPageSlice(rows, pageId);
  const visibleRows = pageRows.filter(row => matching.has(rowId(row))).length;
  return `${tabs()}<div class="subnav">${
    [['data', t('data')], ['structure', t('structure')], ['ddl', 'DDL']]
      .map((
        [id, label],
      ) => `<button class="${state.section === id ? 'active' : ''}" data-section="${id}">${label}</button>`).join('')
  }<span class="spacer"></span><span class="muted small">${h(state.database)}.${h(state.table)}</span></div>${
    state.section === 'data'
      ? `<div class="toolbar"><span class="tag">${
        t('baseTable')
      }</span><input type="search" id="row-filter" aria-label="${t('filterPage')}" placeholder="${
        t('filterPagePlaceholder')
      }" value="${h(state.rowFilter)}">${ib('filter', t('filterSort'), 'filter')}${
        ib('columns', t('columnSettings'), 'table-columns')
      }<span class="spacer"></span>${
        ib(
          'plus',
          t('newRow'),
          'new-row',
          state.connectionModes.mysql ? 'disabled' : '',
        )
      }${ib('download', t('exportData'), 'export')}${
        ib(
          'upload',
          t('importData'),
          'import',
          state.connectionModes.mysql ? 'disabled' : '',
        )
      }${ib('repeat', t('refreshData'), 'refresh-data')}</div>${
        resultPageMarkup(gridMarkup, pageId, rows.length, 100, matching)
      }<div class="table-footer"><span class="selection-count">${
        state.selected.size
          ? t('selectedRows', { count: state.selected.size })
          : ''
      }</span><span class="spacer"></span><span class="selection-help">${
        t('rowSelectionHelp', { modifier: isMac ? '⌘' : 'Ctrl' })
      }</span>${
        ib(
          'copy',
          t('copyCells'),
          'copy-cells',
          state.cellSelection ? '' : 'disabled',
        )
      }${
        ib(
          'info',
          t('cellDetails'),
          'inspect-cell',
          state.menuRow && state.menuCell ? '' : 'disabled',
        )
      }</div>${resultPager(pageId, rows.length, { live: true, visible: visibleRows })}${draftBar()}`
      : state.section === 'structure'
      ? `<div class="table-wrap schema-preview">${
        simpleTable(
          [
            t('field'),
            t('type'),
            'NULL',
            t('constraint'),
            t('columnComment'),
          ],
          Object.keys(databases[state.database][state.table][0]).map(
            c => [
              c,
              c === 'id' || c.endsWith('_id')
                ? 'BIGINT'
                : ['total', 'revenue', 'price'].includes(c)
                ? 'DECIMAL(12,2)'
                : c === 'created_at'
                ? 'DATETIME'
                : 'VARCHAR',
              c === 'email' ? t('yes') : t('no'),
              c === 'id' ? 'PRIMARY KEY · AUTO_INCREMENT' : '—',
              sampleColumnComments[state.database]?.[state.table]?.[c]
                ?? '',
            ],
          ),
        )
      }</div>`
      : ddlPanel()
  }`;
}
function redisKeyRow(key) {
  const label = state.keyTree
    ? key.name.slice(key.name.lastIndexOf(':') + 1) || '∅'
    : key.name;
  return `<button class="key ${state.selectedKeys.has(key) ? 'active' : ''}" data-key="${
    h(key.name)
  }" data-key-index="${keySamples.indexOf(key)}" title="${
    h(key.name)
  } · ${key.type} · ${key.ttl}"><span class="key-name">${
    h(label)
  }</span><span class="key-meta"><span class="key-type">${key.type}</span><span>${key.ttl}</span></span></button>`;
}
function redisKeyList(keys) {
  if (!state.keyTree) return keys.map(redisKeyRow).join('');
  // 目录只组织当前加载的 key；完整前缀作为身份，空分段与同名叶子保持独立。
  const root = { folders: new Map(), leaves: [] };
  for (const key of keys) {
    let node = root, prefix = '';
    const parts = key.name.split(':');
    for (const part of parts.slice(0, -1)) {
      prefix += part + ':';
      if (!node.folders.has(part)) {
        node.folders.set(part, {
          label: part || '∅',
          prefix,
          folders: new Map(),
          leaves: [],
          count: 0,
        });
      }
      node = node.folders.get(part);
      node.count++;
    }
    node.leaves.push(key);
  }
  const renderNode = node =>
    [...node.folders.values()].map(folder => {
      const identity = state.redisDb + '\0' + folder.prefix;
      const expanded = state.expandedKeyFolders.has(identity);
      return `<button class="key-folder row" data-key-folder="${
        h(encodeURIComponent(folder.prefix))
      }" aria-expanded="${expanded}" title="${h(folder.prefix)}">${icon(expanded ? 'chev' : 'chev-r')}${
        h(folder.label)
      }<span class="spacer"></span>${folder.count}</button>${
        expanded
          ? `<div class="key-children">${renderNode(folder)}</div>`
          : ''
      }`;
    }).join('') + node.leaves.map(redisKeyRow).join('');
  return renderNode(root);
}
function redis() {
  const keys = redisScanKeys();
  const key = state.redisDb === 0
    ? keySamples.find(k => k.name === state.key)
    : null;
  return `<div class="redis-browser"><section class="key-browser"><div class="key-controls"><input id="key-filter" aria-label="${
    t('keyPattern')
  }" placeholder="${t('keyPatternPlaceholder')}" value="${
    h(state.keyFilter)
  }"><div class="row"><select id="key-type" aria-label="${t('keyType')}">${
    ['all', 'STR', 'HASH', 'LIST', 'SET', 'ZSET'].map(type =>
      `<option value="${type}" ${type === state.keyType ? 'selected' : ''}>${
        type === 'all' ? t('allTypes') : type
      }</option>`
    ).join('')
  }</select>${ib('repeat', t('refreshKeys'), 'refresh-keys')}</div><div class="row muted small">${
    t('keysLoaded', { count: keys.length })
  }<span class="spacer"></span><div role="group" aria-label="${h(t('listTree'))}"><button class="icon-button ${
    !state.keyTree ? 'active' : ''
  }" data-action="key-list" aria-pressed="${!state.keyTree}" title="${h(t('listView'))}" aria-label="${
    h(t('listView'))
  }">${icon('list')}</button><button class="icon-button ${
    state.keyTree ? 'active' : ''
  }" data-action="key-tree" aria-pressed="${state.keyTree}" title="${h(t('treeView'))}" aria-label="${
    h(t('treeView'))
  }">${icon('tree')}</button></div></div></div><div class="keys">${redisKeyList(keys)}${
    keys.length ? '' : `<div class="empty small">${t('noKeys')}</div>`
  }</div>${redisScanFooter()}</section><section class="key-details">${
    key
      ? `<header class="key-title"><div class="key-name"><h2>${h(key.name)}</h2>${
        ib(
          'edit',
          t('renameKey'),
          'rename-key',
          state.connectionModes.redis ? 'disabled' : '',
        )
      }${ib('copy', t('copyKey'), 'copy-key')}${
        ib(
          'trash',
          t('deleteKey'),
          'delete-key',
          state.connectionModes.redis ? 'disabled' : '',
        )
      }</div><div class="row"><span class="tag">${key.type === 'STR' ? 'STRING' : key.type}</span><span>${
        t(key.sizeKey, { count: key.size })
      }</span><span>TTL ${key.ttl === '∞' ? t('forever') : key.ttl}</span>${
        ib(
          'edit',
          t('editTtl'),
          'ttl',
          state.connectionModes.redis ? 'disabled' : '',
        )
      }</div></header><div class="toolbar">${
        key.type === 'STR'
          ? ['text', 'json', 'hex'].map(format =>
            `<button class="${
              state.redisFormat === format ? 'active' : ''
            }" data-action="format-${format}" aria-pressed="${state.redisFormat === format}">${
              format === 'text' ? t('text') : format.toUpperCase()
            }</button>`
          ).join('')
          : `<span class="muted small">${
            key.type === 'HASH'
              ? 'Fields'
              : key.type === 'LIST'
              ? 'Items'
              : 'Members'
          }</span>${ib('columns', t('columnSettings'), 'redis-columns')}`
      }<span class="spacer"></span>${
        ib(
          'edit',
          t('editValue'),
          'edit-key',
          state.connectionModes.redis ? 'disabled' : '',
        )
      }${
        key.type === 'STR'
          ? ''
          : ib(
            'plus',
            t('addEntry') + ' · ' + (key.type === 'HASH'
              ? t('field')
              : key.type === 'LIST'
              ? t('value')
              : t('member')),
            'add-entry',
            state.connectionModes.redis ? 'disabled' : '',
          ) + ib(
            'trash',
            t('deleteEntry') + ' · ' + (key.type === 'HASH'
              ? t('field')
              : key.type === 'LIST'
              ? t('value')
              : t('member')),
            'delete-entry',
            state.connectionModes.redis || !state.selectedEntries.size
              ? 'disabled'
              : '',
          )
      }${ib('download', t('exportData'), 'export')}${
        ib(
          'upload',
          t('importData'),
          'import',
          state.connectionModes.redis ? 'disabled' : '',
        )
      }${ib('copy', t('copyValue'), 'copy-value')}</div><p class="muted small" style="padding: 0 18px">${
        h(t('copyValueHelp'))
      }</p>${
        key.type === 'STR'
          ? `<pre class="value">${h(redisValue(key))}</pre>`
          : `${
            resultPageMarkup(
              `<div class="table-wrap">${redisRows(key)}</div>`,
              `redis:${state.redisDb}:${key.name}`,
              key.rows.length,
              20,
            )
          }${
            resultPager(`redis:${state.redisDb}:${key.name}`, key.rows.length, {
              size: 20,
              live: true,
              singlePass: true,
            })
          }`
      }${draftBar()}`
      : `<div class="empty">${t('chooseKey', { db: state.redisDb })}</div>`
  }</section></div>${state.cli ? cli() : ''}`;
}

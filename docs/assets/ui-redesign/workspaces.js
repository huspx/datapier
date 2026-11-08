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
  if (tab.schemaCreateKind) return `${tabs()}${schemaCreateWorkspacePanel(tab)}`;
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
  const pageRows = resultPageSlice(rows, pageId);
  const firstVisible = pageRows.find(row => matching.has(rowId(row)));
  const gridMarkup = state.section === 'data' ? grid(firstVisible && rowId(firstVisible)) : '';
  state.rowFilter = savedFilter;
  const visibleRows = pageRows.filter(row => matching.has(rowId(row))).length;
  return `${tabs()}<div class="subnav" role="tablist" aria-label="${h(az('表分区', 'Table sections'))}">${
    [['data', t('data')], ['structure', t('structure')], ['ddl', 'DDL']]
      .map((
        [id, label],
      ) =>
        `<button role="tab" aria-selected="${state.section === id}" tabindex="${
          state.section === id ? '0' : '-1'
        }" class="${state.section === id ? 'active' : ''}" data-section="${id}">${label}</button>`
      ).join('')
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
      }${ib('repeat', t('refreshData'), 'refresh-data')}${
        state.connectionModes.mysql
          ? `<span class="muted small">${
            h(az('只读连接：不能新增或导入数据', 'Read-only connection: adding and importing data are unavailable'))
          }</span>`
          : ''
      }</div>${
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
      ? schemaStructurePanel(tab)
      : ddlPanel()
  }`;
}
let redisTreeFocus = null;
function redisKeyRow(key) {
  const label = state.keyTree
    ? key.name.slice(key.name.lastIndexOf(':') + 1) || '∅'
    : key.name;
  return `<button class="key ${state.selectedKeys.has(key) ? 'active' : ''}" ${
    state.keyTree
      ? `role="treeitem" aria-selected="${state.selectedKeys.has(key)}" tabindex="${
        redisTreeFocus === `key:${state.redisDb}:${key.name}` ? '0' : '-1'
      }"`
      : ''
  } data-key="${h(key.name)}" data-key-index="${keySamples.indexOf(key)}" title="${
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
      const groupId = `redis-tree-${state.redisDb}-${encodeURIComponent(folder.prefix)}`;
      return `<button class="key-folder row" role="treeitem" tabindex="${
        redisTreeFocus === `folder:${state.redisDb}:${folder.prefix}` ? '0' : '-1'
      }" data-key-folder="${h(encodeURIComponent(folder.prefix))}" aria-expanded="${expanded}" ${
        expanded ? `aria-owns="${h(groupId)}"` : ''
      } title="${h(folder.prefix)}">${icon(expanded ? 'chev' : 'chev-r')}${
        h(folder.label)
      }<span class="spacer"></span>${folder.count}</button>${
        expanded
          ? `<div class="key-children" role="group" id="${h(groupId)}">${renderNode(folder)}</div>`
          : ''
      }`;
    }).join('') + node.leaves.map(redisKeyRow).join('');
  const markup = renderNode(root);
  return markup.includes('tabindex="0"') ? markup : markup.replace('tabindex="-1"', 'tabindex="0"');
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
  }">${icon('tree')}</button></div></div></div><div class="keys" ${
    state.keyTree ? `role="tree" aria-label="${h(az('Redis key 树', 'Redis key tree'))}"` : ''
  }>${redisKeyList(keys)}${
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
          ? `<div class="subnav" role="tablist" aria-label="${h(az('值格式', 'Value format'))}">${
            ['text', 'json', 'hex'].map(format =>
              `<button role="tab" tabindex="${state.redisFormat === format ? '0' : '-1'}" class="${
                state.redisFormat === format ? 'active' : ''
              }" data-action="format-${format}" aria-selected="${state.redisFormat === format}">${
                format === 'text' ? t('text') : format.toUpperCase()
              }</button>`
            ).join('')
          }</div>`
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
      }${ib('copy', t('copyValue'), 'copy-value')}${
        state.connectionModes.redis
          ? `<span class="muted small">${
            h(az('只读连接：不能修改或导入数据', 'Read-only connection: modifying and importing data are unavailable'))
          }</span>`
          : ''
      }</div><p class="muted small" style="padding: 0 18px">${h(t('copyValueHelp'))}</p>${
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

function redisTreeIdentity(item) {
  return item.dataset.keyFolder !== undefined
    ? `folder:${state.redisDb}:${decodeURIComponent(item.dataset.keyFolder)}`
    : `key:${state.redisDb}:${item.dataset.key}`;
}
function focusRedisTreeItem(item) {
  if (!item) return;
  const tree = item.closest('[role="tree"]');
  tree.querySelectorAll('[role="treeitem"][tabindex="0"]').forEach(other => {
    other.tabIndex = -1;
  });
  item.tabIndex = 0;
  redisTreeFocus = redisTreeIdentity(item);
  item.focus();
}
document.addEventListener('focusin', event => {
  const item = event.target.closest?.('.keys[role="tree"] [role="treeitem"]');
  if (item) {
    item.closest('[role="tree"]').querySelectorAll('[role="treeitem"][tabindex="0"]').forEach(other => {
      if (other !== item) other.tabIndex = -1;
    });
    item.tabIndex = 0;
    redisTreeFocus = redisTreeIdentity(item);
  }
});
document.addEventListener('click', event => {
  const folder = event.target.closest?.('.keys[role="tree"] [data-key-folder]');
  if (!folder) return;
  event.stopPropagation();
  const prefix = decodeURIComponent(folder.dataset.keyFolder);
  const identity = state.redisDb + '\0' + prefix;
  redisTreeFocus = redisTreeIdentity(folder);
  if (state.expandedKeyFolders.has(identity)) {
    state.expandedKeyFolders.delete(identity);
    for (const key of state.selectedKeys) {
      if (key.name.startsWith(prefix)) state.selectedKeys.delete(key);
    }
    if (state.keyAnchor?.name?.startsWith(prefix)) state.keyAnchor = null;
  } else {
    state.expandedKeyFolders.add(identity);
  }
  render();
  document.querySelector('.keys[role="tree"] [role="treeitem"][tabindex="0"]')?.focus();
}, true);
document.addEventListener('keydown', event => {
  const item = event.target.matches?.('.keys[role="tree"] [role="treeitem"]') ? event.target : null;
  if (!item || event.altKey || event.ctrlKey || event.metaKey || event.isComposing) return;
  if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  const tree = item.closest('[role="tree"]');
  const items = [...tree.querySelectorAll('[role="treeitem"]')];
  const index = items.indexOf(item);
  let next;
  if (event.key === 'ArrowDown') next = items[Math.min(index + 1, items.length - 1)];
  else if (event.key === 'ArrowUp') next = items[Math.max(index - 1, 0)];
  else if (event.key === 'Home') next = items[0];
  else if (event.key === 'End') next = items[items.length - 1];
  else if (event.key === 'ArrowRight' && item.dataset.keyFolder !== undefined) {
    if (item.getAttribute('aria-expanded') === 'false') item.click();
    else next = item.nextElementSibling?.querySelector('[role="treeitem"]');
  } else if (event.key === 'ArrowLeft') {
    if (item.dataset.keyFolder !== undefined && item.getAttribute('aria-expanded') === 'true') item.click();
    else next = item.parentElement.closest('[role="group"]')?.previousElementSibling;
  }
  event.preventDefault();
  focusRedisTreeItem(next);
});

document.addEventListener('keydown', event => {
  const tab = event.target.closest?.('[role="tablist"] > [role="tab"]');
  if (!tab || !tab.closest('.subnav') || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  const tabs = [...tab.parentElement.querySelectorAll(':scope > [role="tab"]')];
  const index = tabs.indexOf(tab);
  const next = event.key === 'Home' ? 0 : event.key === 'End'
    ? tabs.length - 1
    : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
  event.preventDefault();
  tabs.forEach((item, i) => {
    item.tabIndex = i === next ? 0 : -1;
  });
  tabs[next].focus();
});

'use strict';
function openConnectionForm(
  mode,
  provider = mode === 'new-connection'
    ? 'mysql'
    : state.provider || 'mysql',
) {
  if (mode === 'connection-settings') {
    const targets = state.tabs.filter(tab => (tab.provider || 'mysql') === provider);
    if (!closeTabs(targets.map(tab => tab.id))) return;
    state.connectionStatus[provider] = 'disconnected';
  }
  state.connectionForm = {
    provider,
    name: mode === 'new-connection'
      ? ''
      : provider === 'mysql'
      ? 'Commerce'
      : 'Cache',
    host: mode === 'new-connection' ? '127.0.0.1' : 'localhost',
    port: provider === 'mysql' ? '3306' : '6379',
    username: mode === 'new-connection'
      ? ''
      : provider === 'mysql'
      ? 'root'
      : 'default',
    password: '',
    database: '',
    mysqlTimezone: mode === 'new-connection' ? '+00:00' : state.mysqlOptions?.timezone ?? '+00:00',
    mysqlConnectTimeout: mode === 'new-connection' ? '' : state.mysqlOptions?.connectTimeout ?? '',
    mysqlQueryTimeout: mode === 'new-connection' ? '' : state.mysqlOptions?.queryTimeout ?? '',
    redisDb: mode === 'new-connection' ? 0 : state.redisDb,
    redisDatabasesOverride: mode === 'new-connection' ? '' : state.redisDatabasesOverride,
    tls: false,
    ca: '',
    cert: '',
    key: '',
    readOnly: mode === 'new-connection'
      ? true
      : state.connectionModes[provider],
    dbLoaded: mode === 'connection-settings',
    dbSearch: '',
    selectedDatabases: new Set(
      mode === 'new-connection' ? [] : state.visibleDatabases,
    ),
  };
  state.overlay = mode;
  state.menu = null;
  render();
}
function matchingDatabases() {
  return Object.keys(databases).filter(name =>
    name.toLowerCase().includes(state.connectionForm.dbSearch.toLowerCase())
  );
}
function databaseOptions() {
  const form = state.connectionForm, matches = matchingDatabases();
  return `<div class="database-match-actions"><span>${
    t('matches', { count: matches.length })
  }</span><span class="spacer"></span>${ib('check-square', t('selectMatches'), 'db-select-matches')}${
    ib('clear-square', t('clearMatches'), 'db-clear-matches')
  }</div><div class="database-options">${
    matches.map(name =>
      `<label class="database-option"><input type="checkbox" data-database-choice="${h(name)}" ${
        form.selectedDatabases.has(name) ? 'checked' : ''
      }><span>${icon('database')}</span><span>${h(name)}</span></label>`
    ).join('')
    || `<div class="muted small" style="padding:10px 4px">${t('noMatchingDatabases')}</div>`
  }</div>`;
}
function databaseScope() {
  const form = state.connectionForm;
  return `<fieldset class="database-scope"><legend>${
    t('databaseScope')
  }</legend><div class="database-scope-head"><span class="muted" data-db-count>${
    form.dbLoaded
      ? t('selectedCount', {
        count: form.selectedDatabases.size,
        total: Object.keys(databases).length,
      })
      : t('databasesNotLoaded')
  }</span><span class="spacer"></span>${
    ib(
      'repeat',
      t(form.dbLoaded ? 'reload' : 'loadDatabases'),
      'load-databases',
    )
  }</div>${
    form.dbLoaded
      ? `<label for="connection-db-search">${
        t('filterDatabases')
      }</label><input id="connection-db-search" type="search" aria-label="${t('filterDatabases')}" placeholder="${
        t('searchDatabases')
      }" value="${
        h(form.dbSearch)
      }"><div id="connection-db-options">${databaseOptions()}</div><div class="muted small" style="margin-top:8px">${
        t('scopeHelp')
      }</div>`
      : `<div class="muted small">${t('scopeLoadHelp')}</div>`
  }</fieldset>`;
}
function syncDatabaseOptions() {
  const options = $('#connection-db-options');
  if (options) options.innerHTML = databaseOptions();
  const count = document.querySelector('[data-db-count]');
  if (count) {
    count.textContent = t('selectedCount', {
      count: state.connectionForm.selectedDatabases.size,
      total: Object.keys(databases).length,
    });
  }
}
function connectionFormBody() {
  const form = state.connectionForm;
  return `<div class="fields"><fieldset class="provider-field wide"><legend>${
    t('databaseType')
  }</legend><div class="provider-options" role="radiogroup" aria-label="${t('databaseType')}">${
    ['mysql', 'redis', 'archery'].map(provider =>
      `<button type="button" class="provider-option" role="radio" aria-checked="${
        form.provider === provider
      }" tabindex="${form.provider === provider ? '0' : '-1'}" ${
        state.overlay === 'connection-settings' ? 'disabled' : ''
      } ${provider === 'archery' ? 'data-archery="type-archery"' : `data-connection-type="${provider}"`}>${
        provider === 'archery' ? '<img class="archery-mark" src="assets/archery.svg" alt="">' : marker(provider)
      }<span>${provider === 'mysql' ? 'MySQL' : provider === 'redis' ? 'Redis' : 'Archery'}</span></button>`
    ).join('')
  }</div></fieldset><label class="wide">${
    t('name')
  } <span style="color:var(--danger,red)">*</span><input data-connection-field="name" required aria-required="true" value="${
    h(form.name)
  }" placeholder="${t('connectionName')}"></label><label>${
    t('host')
  } <span style="color:var(--danger,red)">*</span><input data-connection-field="host" required aria-required="true" value="${
    h(form.host)
  }"></label><label>${
    t('port')
  } <span style="color:var(--danger,red)">*</span><input data-connection-field="port" required aria-required="true" value="${
    h(form.port)
  }"></label><label>${t('username')}<input data-connection-field="username" value="${
    h(form.username)
  }"></label><label>${t('password')}<input data-connection-field="password" type="password" value="${
    h(form.password)
  }" placeholder="${t('empty')}"></label>${
    form.provider === 'mysql'
      ? `<label class="wide">${t('defaultDatabase')}<input data-connection-field="database" value="${
        h(form.database)
      }" placeholder="${t('defaultDatabaseHelp')}"></label><label class="wide">${
        az('时区（留空使用服务器默认）', 'Time zone (empty uses server default)')
      }<input data-connection-field="mysqlTimezone" value="${
        h(form.mysqlTimezone)
      }" placeholder="SYSTEM / +08:00 / Asia/Shanghai"></label><p class="wide muted small">${
        az(
          'SQL 与结果固定使用 UTF-8。',
          'SQL and results use UTF-8.',
        )
      }</p>${databaseScope()}`
      : redisConnectionPreview(form)
  }<label class="wide connection-switch"><span>${
    h(t('readOnlyConnection'))
  }</span><input type="checkbox" role="switch" data-connection-check="readOnly" ${
    form.readOnly ? 'checked' : ''
  }></label>${connectionTimeouts(form)}<details class="wide"><summary>${
    h(t('tlsDetails'))
  }</summary><label class="connection-switch"><span>${
    h(t('tlsEnable'))
  }</span><input type="checkbox" role="switch" data-connection-check="tls" ${
    form.tls ? 'checked' : ''
  }></label><div class="fields tls-fields" ${form.tls ? '' : 'hidden'}><label class="wide">${
    h(t('caPath'))
  }<input data-connection-field="ca" value="${h(form.ca)}"></label><label>${
    h(t('certPath'))
  }<input data-connection-field="cert" value="${h(form.cert)}"></label><label>${
    h(t('keyPath'))
  }<input data-connection-field="key" value="${h(form.key)}"></label></div></details></div>`;
}
function connectionTimeouts(form) {
  if (form.provider !== 'mysql') return '';
  return `<details class="wide"><summary>${az('超时设置', 'Timeout settings')}</summary><div class="fields">${
    [['mysqlConnectTimeout', az('连接超时（秒，留空继承全局）', 'Connect timeout (seconds; empty inherits global)')], [
      'mysqlQueryTimeout',
      az('执行超时（秒，留空继承全局）', 'Execution timeout (seconds; empty inherits global)'),
    ]]
      .map(([key, label]) =>
        `<label>${h(label)}<input type="number" min="1" max="86400" step="1" data-connection-field="${key}" value="${
          h(form[key])
        }"></label>`
      ).join('')
  }</div></details>`;
}

// 设计稿只生成 SQL 预览；真实候选由原生工作台从服务器加载这些选项。
const databasePreviewCollations = {
  '': [],
  utf8mb4: ['utf8mb4_unicode_ci', 'utf8mb4_general_ci'],
  gbk: ['gbk_chinese_ci', 'gbk_bin'],
};
function updateDatabasePreview() {
  const form = state.newDatabaseForm;
  form.sql = form.name.trim()
    ? 'CREATE DATABASE `' + form.name.replaceAll('`', '``') + '`'
      + (form.charset ? ' CHARACTER SET ' + form.charset : '')
      + (form.collation ? ' COLLATE ' + form.collation : '') + ';'
    : '';
  document.querySelector('#database-create-sql').textContent = form.sql;
  document.querySelector('[data-action="database-create-execute"]').disabled = !form.sql;
}
function newDatabaseDialog() {
  const form = state.newDatabaseForm;
  const field = (key, label, control) => `<label class="wide">${h(label)}${control}</label>`;
  return modal(
    az('新建数据库', 'Create database'),
    `<p class="notice">${
      az(
        '仅预览 SQL；设计稿不连接或写入数据库。',
        'SQL preview only; this prototype does not connect to or write to a database.',
      )
    }</p><div class="fields">${
      field(
        'name',
        az('数据库名称', 'Database name'),
        `<input data-new-database-field="name" maxlength="64" required value="${h(form.name)}">`,
      )
    }${
      field(
        'charset',
        az('数据库字符集', 'Database character set'),
        `<select data-new-database-field="charset">${
          ['', 'utf8mb4', 'gbk'].map(value =>
            `<option value="${value}" ${value === form.charset ? 'selected' : ''}>${
              value || az('服务器默认', 'Server default')
            }</option>`
          ).join('')
        }</select>`,
      )
    }${
      field(
        'collation',
        az('排序规则', 'Collation'),
        `<select data-new-database-field="collation"><option value="">${
          az('字符集默认', 'Character set default')
        }</option>${
          databasePreviewCollations[form.charset].map(value =>
            `<option ${value === form.collation ? 'selected' : ''}>${value}</option>`
          ).join('')
        }</select>`,
      )
    }</div><pre class="preview-code" id="database-create-sql">${h(form.sql || '')}</pre>`,
    `<span class="spacer"></span><button class="button" data-action="close-modal">${
      h(t('cancel'))
    }</button><button class="primary" data-action="database-create-execute" ${form.sql ? '' : 'disabled'}>${
      az('执行', 'Execute')
    }</button>`,
  );
}

const treeGroups = new Set();
document.addEventListener('click', event => {
  const button = event.target.closest('[data-tree-group]');
  if (!button) return;
  event.stopImmediatePropagation();
  const key = button.dataset.treeGroup;
  if (treeGroups.has(key)) treeGroups.delete(key);
  else treeGroups.add(key);
  render();
  [...document.querySelectorAll('[data-tree-group]')].find(item => item.dataset.treeGroup === key)?.focus();
}, true);
function viewResourcePreview() {
  const target = state.viewPreviewTarget;
  if (!target) return '';
  return modal(
    `${az('视图', 'View')} · ${target.database}.${target.name}`,
    `<p class="archery-note">${
      az('只读示例数据；未连接数据库或发送请求。', 'Read-only example data; no database connection or request is made.')
    }</p><h3>${az('结构', 'Structure')}</h3><div class="table-wrap">${
      simpleTable([t('field'), t('type'), 'NULL', az('注释', 'Comment')], [[
        'status',
        'VARCHAR(32)',
        az('是', 'Yes'),
        az('未提供', 'Not provided'),
      ], ['order_count', 'BIGINT', az('否', 'No'), az('未提供', 'Not provided')]])
    }</div><h3>${az('数据', 'Data')}</h3><div class="table-wrap">${
      simpleTable(['status', 'order_count'], [['active', '3'], ['complete', '8']])
    }</div>`,
  );
}
function tree() {
  if (state.connectionStatus.mysql !== 'connected') return '';
  return `<div class="section-heading">${
    t('databases')
  }</div><div class="object-search-row"><input class="object-search" id="object-filter" aria-label="${
    t('filterObjects')
  }" placeholder="${t('filterObjectsPlaceholder')}" value="${h(state.objectFilter)}">${
    ib('repeat', t('refreshResources'), 'refresh-resources')
  }</div>${
    state.visibleDatabases.size
      ? ''
      : `<div class="muted small" style="padding:12px 8px">${t('noDatabases')}</div>`
  }${
    Object.entries(databases).filter(([db]) => state.visibleDatabases.has(db)).map(([db, tables]) =>
      `<button class="tree-line" data-db="${db}" aria-expanded="${state.expanded.has(db)}">${
        icon(state.expanded.has(db) ? 'chev' : 'chev-r')
      }${icon('database')}<span>${db}</span></button>${
        state.expanded.has(db)
          ? ['tables', 'views'].map(group => {
            const key = `${db}:${group}`, expanded = treeGroups.has(key);
            const items = group === 'tables' ? Object.keys(tables) : ['orders_summary'];
            const matches = items.filter(name => name.toLowerCase().includes(state.objectFilter.toLowerCase()));
            return `<button class="tree-line depth-1" data-tree-group="${h(key)}" aria-expanded="${expanded}">${
              icon(expanded ? 'chev' : 'chev-r')
            }${icon(group === 'tables' ? 'table' : 'eye')} ${
              t(group)
            }<span class="count">${items.length}</span></button>${
              expanded
                ? matches.map(name =>
                  group === 'tables'
                    ? `<button class="tree-line depth-2 ${
                      db === state.database && name === state.table ? 'active' : ''
                    }" data-open-table="${h(name)}" data-table-db="${h(db)}">${icon('table')}${h(name)}</button>`
                    : `<button class="tree-line depth-2" data-view-preview="${h(name)}" data-view-db="${h(db)}">${
                      icon('eye')
                    }${h(name)} <span class="muted small">${az('示例视图', 'Example view')}</span></button>`
                ).join('') || `<div class="tree-line depth-2 small">${t('noObjects')}</div>`
                : ''
            }`;
          }).join('')
          : ''
      }`
    ).join('')
  }<div class="section-heading">${t('workspace')}</div><button class="tree-line" data-action="new-query">${
    icon('terminal')
  } ${t('sqlQuery')}</button><button class="tree-line" data-action="history">${icon('history')} ${
    t('queryHistory')
  }</button>`;
}
function connectionCard(provider) {
  const name = provider === 'mysql' ? 'Commerce' : 'Cache';
  const type = provider === 'mysql' ? 'MySQL' : 'Redis';
  const address = provider === 'mysql'
    ? 'localhost:3306'
    : 'localhost:6379';
  const status = state.connectionStatus[provider];
  const label = {
    connected: t('connected'),
    connecting: t('connecting'),
    failed: t('failed'),
    disconnected: t('disconnected'),
  }[status];
  const explanation = status === 'failed'
    ? `${label}：ECONNREFUSED ${address}`
    : label;
  const tooltip = `${type} · ${name} · ${address} · ${explanation}`;
  return `<button class="connection ${
    state.provider === provider ? 'active' : ''
  }" data-provider="${provider}" title="${h(tooltip)}" aria-label="${h(tooltip)}"><span class="connection-symbol">${
    marker(provider)
  }<span class="connection-badge status-${status}" aria-hidden="true"></span></span><span class="connection-copy"><span class="connection-title"><span class="connection-name">${name}</span></span><span class="connection-sub">${address}</span></span></button>`;
}
function sidebar() {
  return `<aside class="sidebar"><button class="brand brand-toggle" data-action="navigation-toggle" title="${
    state.compact ? t('expandSidebar') : t('collapseSidebar')
  }" aria-label="${state.compact ? t('expandSidebar') : t('collapseSidebar')}" aria-expanded="${!state
    .compact}"><img class="brand-mark" src="assets/datapier.svg" alt="" aria-hidden="true" draggable="false"><span class="brand-name">Datapier</span></button><div class="sidebar-scroll"><div class="section-heading connections-head"><span class="connection-heading">${
    t('connections')
  }</span><span class="spacer connection-heading"></span>${
    previewSource === 'archery'
      ? ab(
        icon('plus'),
        'wizard',
        `class="icon-button" title="${h(t('newConnection'))}" aria-label="${h(t('newConnection'))}"`,
      )
      : ib('plus', t('newConnection'), 'new-connection')
  }</div>${
    previewSource === 'archery'
      ? archeryConnectionCards()
      : ['mysql', 'redis'].filter(provider => !state.previewEmpty && state.connections.has(provider)).map(
        connectionCard,
      ).join('')
  }<div class="nav-content">${
    previewSource === 'archery'
      ? archeryTree()
      : !state.provider || !state.connections.has(state.provider)
          || state.connectionStatus[state.provider] !== 'connected'
      ? ''
      : state.provider === 'mysql'
      ? tree()
      : `<div class="section-heading">${
        t('workspace')
      }</div><button class="tree-line active" data-action="key-browser">${icon('layers')} ${
        t('keyBrowser')
      }</button><button class="tree-line" data-action="cli-toggle">${icon('terminal')} ${
        t('commandPanel')
      }</button><div class="notice small" style="margin:12px 0">${t('redisDbNotice')}</div>`
  }</div></div><div class="sidebar-bottom"><button class="sidebar-settings" data-action="app-settings" title="${
    h(t('settings'))
  }" aria-label="${h(t('settings'))}">${icon('gear')}<span class="settings-text">${
    h(t('settings'))
  }</span></button><button class="sidebar-settings" data-action="shortcut-help" title="${
    h(t('shortcutHelp'))
  }" aria-label="${h(t('shortcutHelp'))}">${icon('keyboard')}<span class="settings-text">${
    h(t('shortcutHelp'))
  }</span></button><button class="sidebar-settings" data-action="exit" title="${h(t('exit'))}" aria-label="${
    h(t('exit'))
  }">${icon('power')}<span class="settings-text">${
    h(t('exitTitle'))
  }</span></button><div class="nav-footer"><span class="dot"></span>${t('localWorkspace')}<span style="float:right">${
    t('saved')
  }</span></div></div></aside>`;
}
function context() {
  const redis = state.provider === 'redis';
  return `<header class="context">${marker(state.provider)}<strong>${redis ? 'Cache' : 'Commerce'}</strong>${
    redis
      ? `<select id="redis-db" class="db-select" aria-label="${t('redisDatabase')}">${
        redisDbOptions(state.redisDb, redisEffectiveCount())
      }</select>${
        redisEffectiveCount() === null
          ? `<span class="muted small">${h(configText('DB 数量未知', 'DB count unknown'))}</span>`
          : ''
      }`
      : `<span class="breadcrumb">/ ${h(state.database)}</span>`
  }<span class="tag secondary">${redis ? 'Redis 8.0' : 'MySQL 8.4'}</span><span class="tag good">${
    t(
      state.connectionModes[state.provider]
        ? 'readOnlyConnection'
        : 'readWrite',
    )
  }</span><span class="spacer"></span></header>`;
}
function tabs(items = state.tabs, active = state.active, archery = false) {
  return `<nav class="tabs" aria-label="${t('tabs')}"><div class="tab-scroll">${
    items.map(tab => {
      const close = archery
        ? `<button class="icon-button" data-archery="close-tab:${tab.id}" aria-label="${
          h(t('closeTab', { title: tab.title }))
        }">${icon('x')}</button>`
        : ib(
          'x',
          t('closeTab', { title: tab.title }),
          'close-tab:' + tab.id,
        );
      const label = `<button class="tab-label" ${
        archery ? `data-archery="tab:${tab.id}"` : `data-tab="${tab.id}"`
      } title="${h(tab.database + '.' + tab.title)}">${
        icon(
          tab.query
            ? 'terminal'
            : tab.provider === 'redis'
            ? 'layers'
            : 'table',
        )
      }<span>${h(tab.title)}</span></button>`;
      return `<div data-tab-container="${tab.id}" class="tab ${tab.id === active ? 'active' : ''}">${
        isMac ? close + label : label + close
      }</div>`;
    }).join('')
  }</div>${
    archery
      ? `<button class="icon-button" data-archery="new-tab" aria-label="${h(t('newQuery'))}">${icon('plus')}</button>`
      : ib('plus', t('newQuery'), 'new-query')
  }</nav>`;
}

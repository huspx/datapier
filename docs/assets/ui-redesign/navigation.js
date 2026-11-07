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
    ['mysql', 'redis'].map(provider =>
      `<button class="provider-option" role="radio" aria-checked="${
        form.provider === provider
      }" data-connection-type="${provider}">${marker(provider)}<span>${
        provider === 'mysql' ? 'MySQL' : 'Redis'
      }</span></button>`
    ).join('')
  }${
    state.overlay === 'new-connection'
      ? `<button class="provider-option" role="radio" aria-checked="false" data-archery="type-archery"><img class="archery-mark" src="assets/archery.svg" alt=""><span>Archery</span></button>`
      : ''
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
      }" placeholder="${t('defaultDatabaseHelp')}"></label>${databaseScope()}`
      : redisConnectionPreview(form)
  }<label class="wide"><span><input type="checkbox" data-connection-check="readOnly" ${
    form.readOnly ? 'checked' : ''
  }> ${h(t('readOnlyConnection'))}</span></label><details class="wide"><summary>${
    h(t('tlsDetails'))
  }</summary><label><span><input type="checkbox" data-connection-check="tls" ${form.tls ? 'checked' : ''}> ${
    h(t('tlsEnable'))
  }</span></label><div class="fields tls-fields" ${form.tls ? '' : 'hidden'}><label class="wide">${
    h(t('caPath'))
  }<input data-connection-field="ca" value="${h(form.ca)}"></label><label>${
    h(t('certPath'))
  }<input data-connection-field="cert" value="${h(form.cert)}"></label><label>${
    h(t('keyPath'))
  }<input data-connection-field="key" value="${h(form.key)}"></label></div></details></div>`;
}
function tree() {
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
          ? `<div class="tree-line depth-1">${icon('chev')}${icon('table')} ${t('tables')}<span class="count">${
            Object.keys(tables).length
          }</span></div>${
            Object.keys(tables).filter(table => table.includes(state.objectFilter.toLowerCase())).map(table =>
              `<button class="tree-line depth-2 ${
                db === state.database && table === state.table
                  ? 'active'
                  : ''
              }" data-open-table="${table}" data-table-db="${db}">${icon('table')}${table}</button>`
            ).join('')
          }${
            !Object.keys(tables).some(table => table.includes(state.objectFilter.toLowerCase()))
              ? `<div class="tree-line depth-2 small">${t('noObjects')}</div>`
              : ''
          }<div class="tree-line depth-1">${icon('chev-r')}${icon('eye')} ${
            t('views')
          }<span class="count">0</span></div>`
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
      : !state.provider
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

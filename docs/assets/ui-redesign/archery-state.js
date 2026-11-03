'use strict';
let previewSource = 'direct';
const archeryPreviewLabels = {
  'archery-server': ['Archery · 服务器管理', 'Archery · Server management'],
  'archery-server-edit': ['Archery · 编辑服务器', 'Archery · Edit server'],
  'archery-preview': ['Archery · 连接预览', 'Archery · Connection preview'],
  'archery-edit': ['Archery · 编辑连接', 'Archery · Edit connection'],
  'archery-management-busy': [
    'Archery · 其他窗口占用',
    'Archery · In use by another window',
  ],
  'archery-management-missing': [
    'Archery · 实例不可见',
    'Archery · Instance unavailable',
  ],
  'archery-login': ['Archery · 服务登录', 'Archery · Service login'],
  'archery-totp': ['Archery · 验证码', 'Archery · Verification code'],
  'archery-instances': ['Archery · 选择实例', 'Archery · Select instances'],
  'archery-query': ['Archery · 查询结果', 'Archery · Query result'],
  'archery-redis': ['Archery · Redis 命令', 'Archery · Redis command'],
  'archery-history': ['Archery · 平台历史', 'Archery · Platform history'],
  'archery-check': ['Archery · 上线 SQL 检查', 'Archery · SQL review'],
  'archery-auth': [
    'Archery · 认证失效',
    'Archery · Authentication expired',
  ],
  'archery-denied': ['Archery · 权限拒绝', 'Archery · Permission denied'],
  'archery-running': ['Archery · 任务运行中', 'Archery · Task running'],
  'archery-unknown': [
    'Archery · 远端状态未知',
    'Archery · Remote state unknown',
  ],
};
const az = (zh, en) => language === 'zh-CN' ? zh : en;
const ai = [
  {
    id: 'mysql-a',
    name: 'mysql-commerce',
    provider: 'mysql',
    dbs: ['commerce', 'analytics'],
  },
  {
    id: 'mysql-b',
    name: 'mysql-reporting',
    provider: 'mysql',
    dbs: ['reports', 'warehouse'],
  },
  {
    id: 'redis-a',
    name: 'redis-cache',
    provider: 'redis',
    dbs: ['0', '1', '2'],
  },
];
const archeryQuerySamples = Array.from({ length: 137 }, (_, n) => [
  1001 + n,
  n % 3 === 1 ? 'pending' : 'paid',
  n === 0 ? '128.00' : n === 1 ? '49.00' : ((n + 1) * 7.25).toFixed(2),
]);
const archeryCliSamples = [
  {
    command: 'GET example:key',
    database: '0',
    instance: 'redis-cache',
    status: 'success',
    ms: 2,
    text: 'example:value\nexample:other',
  },
  {
    command: 'TTL example:key',
    database: '0',
    instance: 'redis-cache',
    status: 'success',
    ms: 1,
    text: '-1',
  },
];
const ap = {
  modal: false,
  mode: 'new',
  step: 'instances',
  challenge: false,
  reauth: false,
  otpVisible: false,
  name: 'Example service',
  url: 'https://archery.example.invalid',
  username: '',
  password: '',
  ca: '',
  otp: '',
  saveSecret: false,
  selectedInstances: new Set(['mysql-a', 'redis-a']),
  instanceSearch: '',
  selected: { 'mysql-a': ['commerce'], 'mysql-b': [], 'redis-a': ['0'] },
  defaults: { 'mysql-a': 'commerce', 'redis-a': '0' },
  instance: 'mysql-a',
  db: 'commerce',
  view: 'query',
  sql: 'SELECT id, status, total FROM orders;',
  command: 'GET example:key',
  cliEntries: [],
  result: true,
  review: false,
  reviewDetail: false,
  requestLimit: 100,
  executedLimit: 100,
  cachedRows: archeryQuerySamples.slice(0, 100),
  resultPage: 0,
  resultPageSize: 20,
  filter: '',
  historyApplied: '',
  page: 0,
  pageSize: 20,
  selectedHistory: 0,
  check: 'complete',
  task: 'idle',
  search: '',
  draft: false,
  tabs: [{
    id: 1,
    number: 1,
    title: 'Query 1',
    database: 'commerce',
    query: true,
  }],
  active: 1,
  nextTab: 2,
  expandedDatabases: new Set(['commerce']),
};
const servers = [{
  id: 's1',
  name: 'Example service',
  url: 'https://archery.example.invalid',
  username: 'demo_user',
  ca: '',
  totp: false,
  totpMode: 'manual',
  passwordSaved: true,
  requiresOtp: true,
  loggedIn: true,
}, {
  id: 's2',
  name: 'Reporting service',
  url: 'https://reporting.example.invalid',
  username: 'analyst',
  ca: '',
  totp: false,
  totpMode: 'manual',
  passwordSaved: true,
  requiresOtp: false,
  loggedIn: true,
}];
const archeryConnections = ai.filter(item => item.id !== 'mysql-b').map(
  item => ({
    server: 's1',
    instance: item.id,
    instanceName: item.name,
    provider: item.provider,
    name: item.name,
    dbs: [...item.dbs],
    defaultDb: item.dbs[0],
  }),
);
let serverId = 's1',
  serverForm = null,
  serverReturn = 'wizard',
  serverDelete = null,
  authStage = null,
  authReturn = null;
let connectionDelete = null;
let activeServerId = 's1';
const connectionNames = Object.fromEntries(
  ai.map(item => [item.id, item.name]),
);
const ab = (label, action, attrs = '') => `<button data-archery="${action}" ${attrs}>${label}</button>`;
const aib = (name, label, action, extra = '') =>
  ab(
    icon(name),
    action,
    `class="icon-button" title="${h(label)}" aria-label="${h(label)}" ${extra}`,
  );
const archeryWorkspaces = new Map();
function switchArcheryInstance(server, instance) {
  if (server === activeServerId && instance === ap.instance) return;
  saveArcheryTab();
  const keys = [
    'tabs',
    'active',
    'nextTab',
    'db',
    'sql',
    'command',
    'cliEntries',
    'cliSelected',
    'result',
    'review',
    'view',
    'task',
    'draft',
    'requestLimit',
    'executedLimit',
    'cachedRows',
    'resultPage',
    'resultPageSize',
  ];
  archeryWorkspaces.set(
    `${activeServerId}:${ap.instance}`,
    Object.fromEntries(keys.map(key => [key, ap[key]])),
  );
  activeServerId = server;
  ap.instance = instance;
  const db = archeryConnections.find(c => c.server === server && c.instance === instance)?.defaultDb
    || ai.find(item => item.id === instance).dbs[0];
  Object.assign(
    ap,
    archeryWorkspaces.get(`${server}:${instance}`) || {
      tabs: [{
        id: 1,
        number: 1,
        title: 'Query 1',
        database: db,
        query: true,
      }],
      active: 1,
      nextTab: 2,
      db,
      sql: '',
      command: '',
      cliEntries: [],
      result: false,
      review: false,
      view: 'query',
      task: 'idle',
      draft: false,
      requestLimit: 100,
      executedLimit: 100,
      cachedRows: [],
      resultPage: 0,
      resultPageSize: 20,
    },
  );
  if (ap.view === 'history') ap.view = ap.historyReturn || 'query';
}
function saveArcheryTab() {
  const tab = ap.tabs.find(item => item.id === ap.active);
  if (tab) {
    Object.assign(tab, {
      sql: ap.sql,
      command: ap.command,
      cliEntries: ap.cliEntries,
      cliSelected: ap.cliSelected,
      result: ap.result,
      review: ap.review,
      requestLimit: ap.requestLimit,
      executedLimit: ap.executedLimit,
      cachedRows: ap.cachedRows,
      resultPage: ap.resultPage,
      resultPageSize: ap.resultPageSize,
      view: ['schema', 'ddl'].includes(ap.view) ? ap.view : 'query',
    });
  }
}
function activateArcheryTab(tab) {
  ap.active = tab?.id ?? null;
  if (tab?.database) ap.db = tab.database;
  ap.sql = tab?.sql ?? '';
  ap.command = tab?.command ?? '';
  ap.cliEntries = tab?.cliEntries ?? [];
  ap.cliSelected = tab?.cliSelected;
  ap.result = tab?.result ?? false;
  ap.review = tab?.review ?? false;
  ap.view = tab?.view ?? (tab?.query === false ? 'schema' : 'query');
  ap.requestLimit = tab?.requestLimit ?? 100;
  ap.executedLimit = tab?.executedLimit ?? 100;
  ap.cachedRows = tab?.cachedRows ?? [];
  ap.resultPage = tab?.resultPage ?? 0;
  ap.resultPageSize = tab?.resultPageSize ?? 20;
}
function archeryConnectionCards() {
  return archeryConnections.map(c => {
    const i = ai.find(item => item.id === c.instance),
      s = servers.find(item => item.id === c.server);
    return `<button class="connection ${
      i.id === ap.instance && c.server === activeServerId ? 'active' : ''
    }" data-archery="instance:${c.server}:${i.id}" data-archery-connection="${i.id}" data-connection-server="${
      h(c.server)
    }" title="${
      h(`${c.name} · ${i.provider.toUpperCase()} · Archery · ${s?.name || ''} · ${i.name} · ${t('connected')}`)
    }" aria-label="${
      h(`${c.name} · ${i.provider.toUpperCase()} · Archery · ${s?.name || ''} · ${i.name} · ${t('connected')}`)
    }"><span class="connection-symbol">${
      marker(i.provider)
    }<img class="archery-source-badge" src="assets/archery-badge.svg?v=d6b5886d" alt="" aria-hidden="true"><span class="connection-badge status-connected" aria-hidden="true"></span></span><span class="connection-copy"><span class="connection-title"><span class="connection-name">${
      h(c.name)
    }</span></span><span class="connection-sub">${h(s?.name || '')} · ${h(i.name)}</span></span></button>`;
  }).join('');
}
function archeryTree() {
  const i = ai.find(item => item.id === ap.instance);
  if (i.provider === 'redis') return '';
  return `<div class="object-search-row"><input class="object-search" data-af="objectFilter" aria-label="${
    h(t('filterObjects'))
  }" placeholder="${h(t('filterObjectsPlaceholder'))}" value="${h(ap.objectFilter || '')}"></div>${
    (archeryConnections.find(c => c.server === activeServerId && c.instance === i.id)?.dbs || i.dbs).map(db => {
      const open = ap.expandedDatabases.has(db);
      return `<button class="tree-line ${db === ap.db ? 'active' : ''}" data-archery="tree:${
        h(db)
      }" aria-expanded="${open}">${icon(open ? 'chev' : 'chev-r')}${icon('database')}<span>${h(db)}</span></button>${
        open && 'orders'.includes((ap.objectFilter || '').toLowerCase())
          ? `<button class="tree-line depth-1" data-archery="table:${h(db)}">${icon('table')} orders</button>`
          : ''
      }`;
    }).join('')
  }<div class="section-heading">${t('workspace')}</div><button class="tree-line" data-archery="new-tab">${
    icon('terminal')
  } ${t('sqlQuery')}</button><button class="tree-line" data-archery="history">${icon('history')} ${
    t('queryHistory')
  }</button>`;
}
const platformHistorySamples = Array.from(
  { length: 125 },
  (_, n) => ({
    text: `SELECT ${n + 1} AS platform_example FROM orders;`,
    server: n % 2 ? 's2' : 's1',
    instance: n % 2 ? 'mysql-reporting' : 'mysql-commerce',
    db: n % 2 ? 'reports' : 'commerce',
  }),
);
const ahistory = () =>
  ap.historyApplied === null
    ? []
    : platformHistorySamples.filter(r =>
      r.server === activeServerId
      && (r.text + r.instance + r.db).toLowerCase().includes(
        ap.historyApplied.toLowerCase(),
      )
    );

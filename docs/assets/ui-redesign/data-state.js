'use strict';
const orders = [
  {
    id: '10421',
    order_no: 'ORD-10421',
    customer_id: '1',
    status: 'paid',
    total: '268.50',
    created_at: '2026-10-28 09:41:12',
  },
  {
    id: '10422',
    order_no: 'ORD-10422',
    customer_id: '2',
    status: 'pending',
    total: '128.00',
    created_at: '2026-10-28 09:43:20',
  },
  {
    id: '10423',
    order_no: 'ORD-10423',
    customer_id: '3',
    status: 'paid',
    total: '499.00',
    created_at: '2026-10-28 09:44:01',
  },
  {
    id: '10424',
    order_no: 'ORD-10424',
    customer_id: '1',
    status: 'refunded',
    total: '89.90',
    created_at: '2026-10-28 09:45:55',
  },
];
const databases = {
  commerce: {
    orders,
    customers: [
      { id: '1', name: '林晓', email: 'lin@example.com', tier: 'pro' },
      { id: '2', name: '许宁', email: 'xu@example.com', tier: 'standard' },
      { id: '3', name: '周末', email: null, tier: 'pro' },
    ],
    order_items: [
      {
        id: '5101',
        order_id: '10421',
        sku: 'SKU-104',
        quantity: '2',
        price: '134.25',
      },
      {
        id: '5102',
        order_id: '10422',
        sku: 'SKU-208',
        quantity: '1',
        price: '128.00',
      },
    ],
  },
  analytics: {
    daily_orders: [{
      id: '201',
      day: '2026-10-28',
      orders: '42',
      revenue: '12680.50',
    }],
  },
};
const keySamples = [
  {
    name: 'user:1042',
    type: 'HASH',
    ttl: '∞',
    size: 5,
    sizeKey: 'sizeFields',
    rows: [
      ['name', '林晓'],
      ['email', 'lin@example.com'],
      ['plan', 'pro'],
      ['region', 'ap-southeast-1'],
      ['updated_at', '2026-10-28T09:41:12Z'],
    ],
  },
  {
    name: 'session:usr:1042',
    type: 'STR',
    ttl: '≈59m',
    size: 94,
    sizeKey: 'sizeBytes',
    value: '{"user_id":1042,"name":"林晓","roles":["admin","editor"],"active":true}',
  },
  {
    name: 'queue:email',
    type: 'LIST',
    ttl: '∞',
    size: 3,
    sizeKey: 'sizeElements',
    rows: [['0', '{"to":"lin@example.com","event":"welcome"}'], [
      '1',
      '{"to":"xu@example.com","event":"receipt"}',
    ], ['2', '{"to":"xu@example.com","event":"receipt"}']],
  },
  {
    name: 'flags:enabled',
    type: 'SET',
    ttl: '∞',
    size: 3,
    sizeKey: 'sizeMembers',
    rows: [['billing_v2'], ['checkout_v3'], ['email_digest']],
  },
  {
    name: 'ranking:weekly',
    type: 'ZSET',
    ttl: '≈6d',
    size: 3,
    sizeKey: 'sizeMembers',
    rows: [['alice', '1250.5'], ['lin', '1180'], ['bob', '1100']],
  },
  {
    name: 'cache:binary',
    type: 'STR',
    ttl: '≈15m',
    size: 8,
    sizeKey: 'sizeBytes',
    value: '\\x00\\xFFbinary',
    bytes: new Uint8Array([0, 255, 98, 105, 110, 97, 114, 121]),
  },
  {
    name: 'app:cache:session:42',
    type: 'STR',
    ttl: '≈15m',
    size: 7,
    sizeKey: 'sizeBytes',
    value: 'session',
  },
  {
    name: 'app:cache:token:42',
    type: 'STR',
    ttl: '≈15m',
    size: 5,
    sizeKey: 'sizeBytes',
    value: 'token',
  },
  {
    name: 'app:jobs:email:pending',
    type: 'LIST',
    ttl: '∞',
    size: 1,
    sizeKey: 'sizeElements',
    rows: [['0', 'welcome']],
  },
  {
    name: 'app',
    type: 'STR',
    ttl: '∞',
    size: 4,
    sizeKey: 'sizeBytes',
    value: 'root',
  },
];
const sampleColumnComments = {
  commerce: {
    orders: {
      id: '订单主键',
      order_no: '业务订单号',
      total: '订单金额\n单位：人民币',
    },
    customers: { id: '客户主键', email: '联系邮箱\n允许为空' },
    order_items: { order_id: '所属订单', sku: '商品编码', price: '单价' },
  },
  analytics: { daily_orders: { revenue: '每日销售额' } },
};
const isMac = /^mac/i.test(
  navigator.userAgentData?.platform || navigator.platform || '',
);
// 编辑主键值时仍以首次显示的身份定位原行和行选区。
const rowIds = new WeakMap();
const rowId = row => {
  if (!rowIds.has(row)) rowIds.set(row, row.id ?? row.day);
  return rowIds.get(row);
};
const cellOriginals = new WeakMap();
const deletedRows = new WeakSet();
const state = {
  provider: 'mysql',
  previewEmpty: false,
  drafts: [],
  historyFilter: '',
  historyApplied: '',
  historyPage: 0,
  historyPageSize: 20,
  historySelected: 0,
  redisFormat: 'text',
  keyTree: false,
  expandedKeyFolders: new Set(),
  filters: {},
  redisRow: 0,
  selectedKeys: new Set(),
  keyAnchor: null,
  selectedEntries: new Set(),
  entryAnchor: null,
  redisDeleteTarget: null,
  connectionStatus: { mysql: 'connected', redis: 'connected' },
  connectionModes: { mysql: false, redis: false },
  compact: window.matchMedia('(max-width: 760px)').matches,
  wideCompact: false,
  narrow: window.matchMedia('(max-width: 760px)').matches,
  expanded: new Set(['commerce']),
  database: 'commerce',
  table: 'orders',
  tabs: [
    { id: 1, database: 'commerce', table: 'orders', title: 'orders' },
    { id: 2, database: 'commerce', query: true, title: '订单查询.sql' },
  ],
  active: 1,
  nextTab: 3,
  selected: new Set(),
  anchor: null,
  cellEdit: null,
  cellSelection: null,
  columnWidths: {},
  importPreview: null,
  section: 'data',
  objectFilter: '',
  rowFilter: '',
  hiddenColumns: {},
  sortColumns: {},
  resultFilter: '',
  redisDb: 0,
  redisDetectedCount: 16,
  redisDetection: 'loaded',
  redisDatabasesOverride: '',
  connectionApprovalPolicies: {
    mysql: 'dangerous',
    redis: 'dangerous',
    'archery-mysql': 'dangerous',
    'archery-redis': 'dangerous',
  },
  cliDb: 0,
  key: 'user:1042',
  keyFilter: '',
  keyType: 'all',
  cli: true,
  cliShare: 0.35,
  cliState: 'success',
  cliEntries: [
    {
      command: 'HGETALL user:1042',
      database: '0',
      instance: 'Cache',
      status: 'success',
      ms: 2,
      rows: [['name', 'Example user'], ['email', 'lin@example.com'], [
        'plan',
        'pro',
      ]],
    },
    {
      command: 'TTL user:1042',
      database: '0',
      instance: 'Cache',
      status: 'success',
      ms: 1,
      text: '(integer) -1',
    },
  ],
  cliText: 'HGETALL user:1042',
  cliHistory: false,
  detailKind: 'task',
  overlay: null,
  menu: null,
  sqlCopy: '',
  connectionForm: null,
  connections: new Set(['mysql', 'redis']),
  visibleDatabases: new Set(Object.keys(databases)),
};
const currentRows = () => {
  const sort = state.sortColumns[`${state.database}.${state.table}`];
  const rows = (databases[state.database]?.[state.table] || []).filter(
    row => {
      const filter = state.filters[`${state.database}.${state.table}`];
      const value = filter && row[filter.column];
      return Object.values(row).some(v => String(v).toLowerCase().includes(state.rowFilter.toLowerCase()))
        && (!filter || (filter.operator === 'null'
          ? value === null
          : filter.operator === 'equals'
          ? String(value) === filter.value
          : String(value).toLowerCase().includes(
            filter.value.toLowerCase(),
          )));
    },
  );
  if (sort?.column) {
    rows.sort((a, b) =>
      String(a[sort.column] ?? '').localeCompare(
        String(b[sort.column] ?? ''),
        undefined,
        { numeric: true },
      ) * (sort.desc ? -1 : 1)
    );
  }
  return rows;
};

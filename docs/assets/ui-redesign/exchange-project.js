'use strict';
// 所有状态仅存在本页；演示动作不能产生文件、凭据或数据库请求。
const xp = {
  open: '',
  section: 'sources',
  previewTab: 'structure',
  mode: 'import',
  provider: 'mysql',
  scenario: 'default',
  fields: {
    source: 'orders.csv',
    target: 'Commerce / commerce.orders',
    format: 'CSV',
    delimiter: ',',
    nullMarker: '\\N',
    encoding: 'UTF-8',
    header: true,
    backslashEscape: true,
    csvColumns: 'order_id, amount',
    binaryColumns: '',
    defaults: '',
    recordRanges: '',
    range: 'all',
    prefix: 'cache:',
    targetPrefix: 'copy:',
    conflict: 'stop',
    projectName: '订单测试',
    projectPath: '/example/orders',
    alias: 'orders',
    binding: 'Commerce / commerce',
    query: 'SELECT *\nFROM orders\nLIMIT 100;',
    redisQuery: 'SCAN 0 MATCH cache:*\nGET cache:example',
    transferName: '订单迁移',
    transferConfig: '{\n  "mode": "copy",\n  "source": "orders"\n}',
    layout: '查询与迁移并列',
  },
  endpoints: { source: { connection: '', database: '' }, target: { connection: '', database: '' } },
  catalogState: 'ready',
  newTargetStep: false,
  newTargetName: '',
  mappings: [{ source: '', target: '' }],
  files: ['orders.csv'],
  filters: [''],
  activeObject: 0,
  objects: [{ source: '', target: '', checked: true }],
  selected: {
    difference: false,
  },
  confirmed: false,
  prepared: false,
  bindingConfirmed: false,
  projectDirty: false,
  fileSelection: { bindings: 'current', queries: 'mysql', transfers: 'current' },
  projectActive: false,
  projectBaseline: null,
  savePreview: false,
  conflictDecisions: { added: '', modified: '', deleted: '' },
  documentRatio: 25,
  pendingLeave: null,
  allowLeave: false,
  previousSelect: '',
  message: '',
};
const xpCatalog = {
  mysql: {
    'Commerce MySQL': {
      commerce: { orders: ['id', 'total'], orders_summary: ['id', 'total'] },
      analytics: { sales: ['id', 'amount'] },
    },
    'Warehouse MySQL': { warehouse: { orders: ['id', 'total'], inventory: ['id', 'quantity'] } },
  },
  redis: {
    'Cache Redis': { 'DB 0': { 'cache:example': [] }, 'DB 3': { 'cache:archive': [] } },
    'Sessions Redis': { 'DB 0': { 'session:example': [] }, 'DB 1': { 'session:archive': [] } },
  },
};
const xpFileColumns = ['order_id', 'amount'];
const mcpExample = {
  view: 'mcp',
  selected: 0,
  environment: 0,
  form: '',
  editIndex: -1,
  draft: {},
  error: '',
  projects: [
    {
      name: '订单服务',
      description: '订单应用数据库',
      roots: ['/example/orders'],
      environments: [
        {
          name: '开发',
          type: 'dev',
          description: '日常开发',
          default: true,
          resources: [
            { profile: 'Commerce MySQL', type: 'mysql', database: 'orders_dev', description: '订单数据' },
            { profile: 'Cache Redis', type: 'redis', redisDb: '0', description: '开发缓存' },
            {
              profile: '旧连接（已失效）',
              type: 'mysql',
              database: 'orders_archive',
              description: '需重新关联',
              invalid: true,
            },
          ],
        },
        {
          name: '测试',
          type: 'test',
          description: '集成测试',
          default: false,
          resources: [
            { profile: 'Commerce MySQL', type: 'mysql', database: 'orders_test', description: '测试数据' },
          ],
        },
        { name: '生产', type: 'prod', description: '生产环境', default: false, resources: [] },
      ],
    },
    {
      name: '库存服务',
      description: '库存与缓存',
      roots: ['/example/inventory'],
      environments: [
        { name: '开发', type: 'dev', description: '开发环境', default: true, resources: [] },
      ],
    },
  ],
};
const mcpProfiles = [
  { name: 'Commerce MySQL', type: 'mysql', databases: ['orders_dev', 'orders_test', 'inventory'] },
  { name: 'Cache Redis', type: 'redis', databases: Array.from({ length: 16 }, (_, index) => String(index)) },
];
const xpText = (zh, en) => language === 'zh-CN' ? zh : en;
const xpLabel = (zh, en) => h(xpText(zh, en));
const xpProjectKeys = [
  'projectName',
  'projectPath',
  'layout',
  'alias',
  'query',
  'redisQuery',
  'transferName',
  'transferConfig',
  'externalQuery',
  'oldTransfer',
];
function xpSnapshot() {
  return Object.fromEntries(xpProjectKeys.map(key => [key, xp.fields[key]]));
}
function xpRestore() {
  if (xp.projectBaseline) Object.assign(xp.fields, xp.projectBaseline);
  xp.projectDirty = false;
  xp.savePreview = false;
}
function xpGuardLeave(next) {
  if (xp.open !== 'project' || !xp.projectDirty || xp.allowLeave) return false;
  xp.pendingLeave = next;
  renderExchangeProject();
  return true;
}
function xpFinishLeave(choice) {
  const next = xp.pendingLeave;
  xp.pendingLeave = null;
  if (choice === 'cancel') {
    renderExchangeProject();
    return;
  }
  if (choice === 'discard') xpRestore();
  xp.allowLeave = true;
  try {
    next();
  } finally {
    xp.allowLeave = false;
  }
}
xp.fields.externalQuery = '';
xp.fields.oldTransfer = '{ "mode": "copy" }';
xp.projectBaseline = xpSnapshot();
document.addEventListener('focusin', event => {
  if (['preview-state', 'preview-source'].includes(event.target.id)) xp.previousSelect = event.target.value;
}, true);
function xpField(key, zh, en, multiline = false) {
  const value = h(xp.fields[key]);
  return `<label class="xp-field"><span>${xpLabel(zh, en)}</span>${
    multiline
      ? `<textarea data-xp-field="${key}" rows="4">${value}</textarea>`
      : `<input data-xp-field="${key}" value="${value}">`
  }</label>`;
}
function xpSelect(key, zh, en, choices) {
  return `<label class="xp-field"><span>${xpLabel(zh, en)}</span><select data-xp-field="${key}">${
    choices.map(([value, a, b]) =>
      `<option value="${h(value)}"${xp.fields[key] === value ? ' selected' : ''}>${xpLabel(a, b)}</option>`
    ).join('')
  }</select></label>`;
}
function xpCheck(key, zh, en) {
  return `<label class="xp-choice"><input type="checkbox" data-xp-choice="${key}"${
    xp.selected[key] ? ' checked' : ''
  }> <span>${xpLabel(zh, en)}</span></label>`;
}
function xpTabs(items) {
  return `<div class="xp-tabs ${xp.open}-tabs" role="tablist" aria-label="${
    xpLabel('工作台分区', 'Workbench sections')
  }">${
    items.map(([id, zh, en]) =>
      `<button type="button" role="tab" data-xp-section="${id}" aria-selected="${xp.section === id}" tabindex="${
        xp.section === id ? 0 : -1
      }">${xpLabel(zh, en)}</button>`
    ).join('')
  }</div>`;
}
function xpMenu(attribute, label, value, options, disabled = false) {
  return `<label class="xp-field"><span>${h(label)}</span><select ${attribute}${
    disabled ? ' disabled' : ''
  }><option value="">${xpLabel('请选择', 'Select')}</option>${
    options.map(option => `<option value="${h(option)}"${value === option ? ' selected' : ''}>${h(option)}</option>`)
      .join('')
  }</select></label>`;
}
function xpEndpoint(side) {
  const endpoint = xp.endpoints[side];
  const connections = xpCatalog[xp.provider];
  const databases = connections[endpoint.connection] || {};
  return `${
    xpMenu(
      `data-xp-endpoint="${side}:connection"`,
      xpText(
        side === 'source' ? '来源连接' : '目标连接',
        side === 'source' ? 'Source connection' : 'Target connection',
      ),
      endpoint.connection,
      Object.keys(connections),
    )
  }${
    xpMenu(
      `data-xp-endpoint="${side}:database"`,
      xpText(
        side === 'source' ? '来源数据库' : '目标数据库',
        side === 'source' ? 'Source database' : 'Target database',
      ),
      endpoint.database,
      Object.keys(databases),
      !endpoint.connection || xp.catalogState !== 'ready',
    )
  }`;
}
function xpCollection(key, zh, en) {
  const label = xpText(zh, en);
  const rows = xp[key].map((value, index) =>
    `<div class="xp-collection-row"><input data-xp-collection="${key}:${index}" aria-label="${h(label)} ${
      index + 1
    }" value="${h(value)}">${
      xpIcon('trash', `移除${zh} ${index + 1}`, `Remove ${en} ${index + 1}`, `collection-remove:${key}:${index}`)
    }</div>`
  ).join('');
  return `<div class="xp-group"><div class="xp-section-heading"><h4>${h(label)}</h4>${
    xpIcon('plus', `添加${zh}`, `Add ${en}`, `collection-add:${key}`)
  }</div>${rows || `<p>${xpLabel('暂无项目', 'No items')}</p>`}</div>`;
}
function xpObjects() {
  const source = xpCatalog[xp.provider][xp.endpoints.source.connection]?.[xp.endpoints.source.database] || {};
  const target = xpCatalog[xp.provider][xp.endpoints.target.connection]?.[xp.endpoints.target.database] || {};
  const sourceObjects = ['import', 'sql_restore'].includes(xp.mode) ? xp.files.filter(Boolean) : Object.keys(source);
  const rows = xp.objects.map((item, index) =>
    `<div class="xp-map-row"><input type="checkbox" data-xp-collection-check="${index}" aria-label="${
      xpLabel('选择对象', 'Select object')
    } ${index + 1}"${item.checked ? ' checked' : ''}>${
      xpMenu(
        `data-xp-object="${index}:source"`,
        xpText('来源对象', 'Source object'),
        item.source,
        sourceObjects,
        xp.catalogState !== 'ready',
      )
    }${
      ['export', 'sql_dump'].includes(xp.mode)
        ? ''
        : xpMenu(
          `data-xp-object="${index}:target"`,
          xpText('目标对象', 'Target object'),
          item.target,
          Object.keys(target),
          xp.catalogState !== 'ready',
        )
    }<button data-xp-action="mapping-object:${index}" aria-pressed="${xp.activeObject === index}">${
      xpLabel('字段', 'Columns')
    }</button>${xpIcon('trash', '移除对象', 'Remove object', `collection-remove:objects:${index}`)}</div>`
  ).join('');
  const object = xp.objects[xp.activeObject] || {};
  const sourceColumns = ['import', 'sql_restore'].includes(xp.mode)
    ? (object.source ? xpFileColumns : [])
    : source[object.source] || [];
  const targetColumns = target[object.target] || [];
  const mappings = xp.mappings.map((item, index) =>
    `<div class="xp-map-row">${
      xpMenu(
        `data-xp-mapping="${index}:source"`,
        xpText('来源字段', 'Source column'),
        item.source,
        sourceColumns,
        !sourceColumns.length,
      )
    }${
      xpMenu(
        `data-xp-mapping="${index}:target"`,
        xpText('目标字段', 'Target column'),
        item.target,
        targetColumns,
        !targetColumns.length,
      )
    }${xpIcon('trash', '移除字段映射', 'Remove column mapping', `collection-remove:mappings:${index}`)}</div>`
  ).join('');
  return `<div class="xp-group"><div class="xp-section-heading"><h4>${xpLabel('对象映射', 'Object mappings')}</h4>${
    xpIcon('plus', '添加对象', 'Add object', 'collection-add:objects')
  }</div>${rows}${
    xp.provider === 'mysql' && !['export', 'sql_dump'].includes(xp.mode)
      ? `<button data-xp-action="new-target">${xpLabel('新建目标表', 'Create target table')}</button>${
        xp.newTargetStep
          ? `<div class="xp-group"><label class="xp-field"><span>${
            xpLabel('新表名称（示例）', 'New table name (example)')
          }</span><input data-xp-new-target value="${
            h(xp.newTargetName)
          }"></label><div class="xp-actions"><button data-xp-action="new-target-cancel">${
            xpLabel('取消', 'Cancel')
          }</button><button data-xp-action="new-target-save">${
            xpLabel('添加示例目标', 'Add example target')
          }</button></div></div>`
          : ''
      }`
      : ''
  }</div>${
    xp.provider === 'mysql' && !['export', 'sql_dump'].includes(xp.mode)
      ? `<div class="xp-group"><div class="xp-section-heading"><h4>${xpLabel('字段映射', 'Column mappings')}</h4>${
        xpIcon('plus', '添加字段映射', 'Add column mapping', 'collection-add:mappings')
      }</div>${mappings}</div>`
      : ''
  }`;
}
function xpExchangeBody() {
  const mysql = xp.provider === 'mysql',
    csv = xp.fields.format === 'CSV' && ['import', 'export'].includes(xp.mode);
  if (xp.scenario === 'loading') {
    return `<p role="status">${
      xpLabel('正在加载示例对象… 未发送请求。', 'Loading example objects… No request was sent.')
    }</p>`;
  }
  if (xp.scenario === 'empty') {
    return `<p>${
      xpLabel('没有示例对象。请选择其他预览状态。', 'No example objects. Choose another preview state.')
    }</p>`;
  }
  if (xp.section === 'sources') {
    return `<h3>${xpLabel('来源与目标', 'Source and target')}</h3><div class="xp-grid"><label class="xp-field"><span>${
      xpLabel('操作', 'Operation')
    }</span><select data-xp-mode>${
      [
        ['import', '导入', 'Import'],
        ['export', '导出', 'Export'],
        ['copy', '直接迁移', 'Direct copy'],
        ['sql_dump', 'SQL 导出', 'SQL dump'],
        ['sql_restore', 'SQL 恢复', 'SQL restore'],
      ].map(([v, a, b]) =>
        `<option value="${v}"${xp.mode === v ? ' selected' : ''}${!mysql && v.startsWith('sql_') ? ' disabled' : ''}>${
          xpLabel(a, b)
        }</option>`
      ).join('')
    }</select></label><label class="xp-field"><span>${
      xpLabel('数据库类型', 'Database type')
    }</span><select data-xp-provider><option value="mysql"${
      mysql ? ' selected' : ''
    }>MySQL</option><option value="redis"${
      !mysql ? ' selected' : ''
    }>Redis</option></select></label></div><div class="xp-group"><h4>${
      xpLabel('来源与目标', 'Source and target')
    }</h4><div class="xp-grid">${['import', 'sql_restore'].includes(xp.mode) ? '' : xpEndpoint('source')}${
      ['export', 'sql_dump'].includes(xp.mode) ? xpField('target', '目标文件', 'Target file') : xpEndpoint('target')
    }${
      ['import', 'export'].includes(xp.mode)
        ? xpSelect('format', '格式', 'Format', [['CSV', 'CSV', 'CSV'], ['JSON', '类型化 JSON', 'Typed JSON']])
        : ''
    }</div></div>${
      ['import', 'sql_restore'].includes(xp.mode) ? xpCollection('files', '输入文件', 'Input files') : ''
    }<div class="xp-group"><label class="xp-field"><span>${
      xpLabel('示例元数据状态', 'Example metadata state')
    }</span><select data-xp-catalog><option value="ready"${xp.catalogState === 'ready' ? ' selected' : ''}>${
      xpLabel('示例数据', 'Example data')
    }</option><option value="loading"${xp.catalogState === 'loading' ? ' selected' : ''}>${
      xpLabel('加载中', 'Loading')
    }</option><option value="failed"${xp.catalogState === 'failed' ? ' selected' : ''}>${
      xpLabel('加载失败', 'Failed')
    }</option></select></label>${
      xp.catalogState === 'loading'
        ? `<p role="status">${xpLabel('正在加载示例元数据。', 'Loading example metadata.')}</p>`
        : xp.catalogState === 'failed'
        ? `<p role="alert">${xpLabel('示例元数据加载失败。', 'Example metadata failed to load.')} ${
          xpIcon('repeat', '重试示例元数据', 'Retry example metadata', 'catalog-retry')
        }</p>`
        : ''
    }</div><p class="muted">${
      xpLabel(
        '路径与连接均为本页示例，不读取文件或连接数据库。',
        'Paths and connections are examples; no files or databases are accessed.',
      )
    }</p>`;
  }
  if (xp.section === 'objects') {
    return `<h3>${xpLabel('对象与映射', 'Objects and mappings')}</h3>${
      mysql
        ? ''
        : `<div class="xp-grid">${xpField('prefix', '来源 key 前缀', 'Source key prefix')}${
          xpField('targetPrefix', '目标 key 前缀', 'Target key prefix')
        }</div>`
    }${mysql ? xpCollection('filters', '筛选条件', 'Filter conditions') : ''}${xpObjects()}${
      mysql
        ? `<h4>${xpLabel('结构差异', 'Schema differences')}</h4>${
          xpCheck('difference', '添加目标列 total DECIMAL(18,2)', 'Add target column total DECIMAL(18,2)')
        }<p class="muted">${
          xpLabel(
            '未选择必需差异时保留诊断并重新准备；SQL 恢复按整个对象及数据选择。',
            'Unselected required differences remain visible for another preparation; SQL restore selects whole objects and data.',
          )
        }</p>`
        : ''
    }`;
  }
  if (xp.section === 'policy') {
    return `<h3>${xpLabel('策略与格式', 'Policy and format')}</h3><div class="xp-grid">${
      xpSelect('range', '范围', 'Range', [
        ['all', '全部', 'All'],
        ['selected', '已选择', 'Selected'],
        ['filtered', '筛选结果', 'Filtered'],
      ])
    }${xpField('recordRanges', '续作记录区间（首尾均包含）', 'Continuation record ranges (inclusive)')}${
      xpSelect('conflict', '冲突处理', 'Conflict policy', [
        ['stop', '停止', 'Stop'],
        ['skip', '跳过', 'Skip'],
        ['replace', '替换（危险）', 'Replace (dangerous)'],
      ])
    }${
      csv
        ? `${xpField('delimiter', '分隔符（单个 ASCII 字符）', 'Delimiter (one ASCII character)')}${
          xpField('nullMarker', 'NULL 标记（允许空）', 'NULL marker (empty allowed)')
        }${xpField('encoding', '编码', 'Encoding')}${
          ['csvColumns', 'binaryColumns'].map(key =>
            `<label class="xp-field"><span>${
              key === 'csvColumns'
                ? xpLabel('无表头时的列顺序', 'Column order without a header')
                : xpLabel('二进制列', 'Binary columns')
            }</span><select multiple data-xp-columns="${key}">${
              xpFileColumns.map(column =>
                `<option value="${h(column)}"${
                  xp.fields[key].split(',').map(value => value.trim()).includes(column) ? ' selected' : ''
                }>${h(column)}</option>`
              ).join('')
            }</select></label>`
          ).join('')
        }${xpField('defaults', '输入列默认值语义', 'Input column default semantics', true)}
        <label class="xp-choice"><input type="checkbox" data-xp-field="header"${xp.fields.header ? ' checked' : ''}> ${
          xpLabel('首行是表头', 'First row is header')
        }</label><label class="xp-choice"><input type="checkbox" data-xp-field="backslashEscape"${
          xp.fields.backslashEscape ? ' checked' : ''
        }> ${xpLabel('反斜杠转义', 'Backslash escaping')}</label>`
        : ''
    }</div><p>${
      xpLabel(
        '取消不保证回滚已生效的批次；未知结果必须先核对。',
        'Cancellation does not roll back applied batches; reconcile unknown outcomes first.',
      )
    }</p>`;
  }
  if (xp.section === 'preview' && !xp.prepared) {
    return `<h3>${xpLabel('准备预览', 'Prepared preview')}</h3><p>${
      xpLabel(
        '配置已改变或尚未准备；请先选择准备预览。',
        'Configuration changed or has not been prepared; select Prepare preview first.',
      )
    }</p>`;
  }
  if (xp.section === 'preview') {
    const tabs = [
      ['structure', '结构', 'Structure'],
      ['sample', '样本', 'Sample'],
      ['diagnostics', '诊断', 'Diagnostics'],
    ];
    return `<h3>${
      xpLabel('准备预览 · 示例', 'Prepared preview · example')
    }</h3><div class="xp-tabs xp-preview-tabs" role="tablist" aria-label="${xpLabel('预览详情', 'Preview details')}">${
      tabs.map(([id, zh, en]) =>
        `<button role="tab" data-xp-preview="${id}" aria-selected="${xp.previewTab === id}">${xpLabel(zh, en)}</button>`
      ).join('')
    }</div>${
      xp.previewTab === 'structure'
        ? xpCheck('difference', '包含结构差异：添加 total 列', 'Include schema difference: add total column')
        : xp.previewTab === 'sample'
        ? `<p>${
          xpLabel(
            '样本仅用于展示，后续记录仍可能转换失败。',
            'Samples are illustrative; later records may fail conversion.',
          )
        }</p><div class="xp-table"><table><thead><tr><th>order_id · BIGINT</th><th>amount · DECIMAL</th><th>${
          xpLabel('目标', 'Target')
        }</th></tr></thead><tbody><tr><td>10421</td><td>268.50</td><td>orders</td></tr><tr><td>10422</td><td>128.00</td><td>orders</td></tr></tbody></table></div>`
        : `<p>${
          xpLabel(
            '示例：检查目标列与对象依赖；未发送请求。',
            'Example: inspect target columns and object dependencies; no request sent.',
          )
        }</p>`
    }<label class="xp-choice"><input type="checkbox" data-xp-confirm${xp.confirmed ? ' checked' : ''}> ${
      xpLabel('我已单独确认所选危险变更及目标', 'I separately confirm the selected dangerous changes and target')
    }</label>`;
  }
  const statuses = {
    running: ['运行中：示例进度 1 / 3；可取消等待。', 'Running: example progress 1 / 3; cancellation is available.'],
    cancelled: [
      '取消后示例：一批已生效，一批未发送；不表示回滚。',
      'Example after cancellation: one batch applied, one not sent; no rollback is implied.',
    ],
    unknown: [
      '示例：写入结果未知，先核对目标，禁止自动重放。',
      'Example: write outcome unknown; reconcile target before any retry.',
    ],
    failed: [
      '示例：一项失败，已生效和未发送项分开；核对后可选择续作。',
      'Example: one item failed; applied and unsent items are separate. Reconcile before selecting a continuation.',
    ],
    default: [
      '尚无报告；可通过预览状态选择示例。',
      'No report yet; choose an example from the preview state selector.',
    ],
  };
  const text = statuses[xp.scenario] || statuses.default;
  return `<h3>${xpLabel('运行报告', 'Run report')}</h3><p role="status">${xpLabel(...text)}</p>${
    xp.scenario === 'default'
      ? ''
      : `<div class="xp-table"><table><thead><tr><th>${xpLabel('项目', 'Item')}</th><th>${
        xpLabel('状态', 'Status')
      }</th></tr></thead><tbody><tr><td>orders</td><td>${
        xp.scenario === 'unknown'
          ? xpLabel('示例 · 待确认', 'Example · unknown')
          : xp.scenario === 'running'
          ? xpLabel('示例 · 运行中', 'Example · running')
          : xpLabel('示例 · 已生效', 'Example · applied')
      }</td></tr><tr><td>orders_summary</td><td>${
        xp.scenario === 'failed'
          ? xpLabel('示例 · 失败', 'Example · failed')
          : xpLabel('示例 · 未发送', 'Example · not sent')
      }</td></tr></tbody></table></div>`
  }<details><summary>${xpLabel('诊断与遗留资源', 'Diagnostics and leftover resources')}</summary><p>${
    xpLabel(
      '示例 partial 文件：身份待核对，不代表完整导出。',
      'Example partial file: identity requires verification; it is not a complete export.',
    )
  }</p></details>`;
}
function xpProjectBody() {
  if (xp.scenario === 'loading') {
    return `<p role="status">${
      xpLabel('正在加载项目示例… 未发送请求。', 'Loading project example… No request was sent.')
    }</p>`;
  }
  if (xp.scenario === 'empty' && !['overview', 'local'].includes(xp.section)) {
    return `<p>${
      xpLabel('尚无项目示例；请在概览中创建或打开。', 'No project example; create or open one in Overview.')
    }</p>`;
  }
  if (xp.section === 'overview') {
    return `<h3>${xpLabel('项目概览', 'Project overview')}</h3><p role="status">${
      xp.projectActive
        ? xpLabel('项目示例已打开', 'Project example open')
        : xpLabel('尚未打开项目示例', 'No project example open')
    } · ${
      xp.projectDirty
        ? xpLabel('共享草稿有未确认更改', 'Shared draft has unconfirmed changes')
        : xpLabel('共享草稿无未确认更改', 'No unconfirmed shared draft changes')
    }</p><div class="xp-section-heading"><h4>${xpLabel('打开／新建', 'Open / create')}</h4><div class="xp-actions">${
      xpIcon('plus', '预览创建', 'Preview create', 'create')
    }${xpIcon('folder', '预览打开', 'Preview open', 'open')}</div></div><div class="xp-grid">${
      xpField('projectName', '项目名称', 'Project name')
    }${xpField('projectPath', '项目目录（示例）', 'Project directory (example)')}${
      xpField('layout', '布局', 'Layout')
    }</div><div class="xp-section-heading"><h4>${
      xpLabel('归档导入／导出', 'Archive import / export')
    }</h4><div class="xp-actions">${xpIcon('upload', '预览导入归档', 'Preview archive import', 'archive-import')}${
      xpIcon('download', '预览导出归档', 'Preview archive export', 'archive-export')
    }</div></div><p>${
      xpLabel(
        '归档包含共享项目文件，不包含凭据或本机绑定。导入新目录后必须重新确认绑定；原目录重开沿用已确认绑定。',
        'Archives contain shared project files, never credentials or local bindings. Confirm bindings again after import into a new directory; reopening the original directory retains confirmed bindings.',
      )
    }</p>`;
  }
  if (xp.section === 'bindings') {
    return `<h3>${
      xpLabel('逻辑连接与本机绑定', 'Logical connections and local bindings')
    }</h3><div class="xp-file-list"><button data-xp-file="bindings:current" aria-pressed="${
      xp.fileSelection.bindings === 'current'
    }"><strong>${h(xp.fields.alias)}</strong><span>MySQL · ${h(xp.fields.binding)} · ${
      xp.bindingConfirmed ? xpLabel('已确认', 'Confirmed') : xpLabel('待确认', 'Unconfirmed')
    }</span></button><button data-xp-file="bindings:invalid" aria-pressed="${
      xp.fileSelection.bindings === 'invalid'
    }"><strong>${xpLabel('旧连接示例', 'Previous connection example')}</strong><span>${
      xpLabel('连接已失效', 'Connection unavailable')
    }</span></button></div>${
      xp.fileSelection.bindings === 'invalid'
        ? `<p role="status">${
          xpLabel('连接已失效，请重新选择本机连接。', 'Connection unavailable; select a local connection again.')
        }</p>`
        : `<h4>${xpLabel('编辑所选绑定', 'Edit selected binding')}</h4><div class="xp-grid">${
          xpField('alias', '逻辑连接 alias', 'Logical connection alias')
        }${xpField('binding', '本机连接 / 数据库', 'Local connection / database')}</div><p>${
          xpLabel(
            '本机绑定只属于本机，不写入项目归档。',
            'Local bindings remain on this machine and are excluded from archives.',
          )
        }</p><label class="xp-choice"><input type="checkbox" data-xp-binding${xp.bindingConfirmed ? ' checked' : ''}> ${
          xpLabel('明确确认此示例绑定', 'Explicitly confirm this example binding')
        }</label><p>${
          xp.bindingConfirmed
            ? xpLabel('示例绑定已在本页确认；未保存。', 'Example binding confirmed on this page; not saved.')
            : xpLabel('绑定待确认，不能作为已连接。', 'Binding requires confirmation; it is not connected.')
        }</p>`
    }`;
  }
  if (xp.section === 'queries') {
    return `<h3>${
      xpLabel('查询', 'Queries')
    }</h3><div class="xp-file-list"><button data-xp-file="queries:mysql" aria-pressed="${
      xp.fileSelection.queries === 'mysql'
    }"><strong>MySQL</strong><code>queries/orders.sql</code></button><button data-xp-file="queries:redis" aria-pressed="${
      xp.fileSelection.queries === 'redis'
    }"><strong>Redis</strong><code>queries/cache.redis</code></button></div><h4>${
      xpLabel('编辑所选查询', 'Edit selected query')
    }</h4><div class="xp-grid">${
      xp.fileSelection.queries === 'mysql'
        ? xpField('query', 'MySQL 查询正文', 'MySQL query text', true)
        : xpField('redisQuery', 'Redis 命令正文', 'Redis command text', true)
    }</div><p>${
      xpLabel(
        '多行文本为项目草稿，切换分区保留；不会执行。',
        'Multiline text is a project draft, retained across sections; it is never executed.',
      )
    }</p><details class="xp-document-preview"><summary>${
      xpLabel('预览 README.md（示例）', 'Preview README.md (example)')
    }</summary><label>${
      xpLabel('标题索引宽度', 'Heading index width')
    } <input type="range" min="15" max="50" value="${xp.documentRatio}" data-xp-document-ratio></label><div class="xp-document-split" style="grid-template-columns: minmax(0, ${xp.documentRatio}fr) minmax(0, ${
      100 - xp.documentRatio
    }fr)"><nav aria-label="${xpLabel('标题索引', 'Heading index')}"><a href="#example-readme-intro">${
      xpLabel('介绍', 'Introduction')
    }</a><a href="#example-readme-query">${
      xpLabel('示例查询', 'Example query')
    }</a></nav><article><h3 id="example-readme-intro">${xpLabel('订单项目', 'Orders project')}</h3><p>${
      xpLabel('这是本页的只读 Markdown 示例，不读取文件。', 'This read-only Markdown example does not read files.')
    }</p><h3 id="example-readme-query">${
      xpLabel('示例查询', 'Example query')
    }</h3><pre><code>SELECT * FROM orders;</code></pre></article></div></details>${
      xp.fields.externalQuery
        ? `<label class="xp-field"><span>queries/new.sql</span><textarea readonly rows="3">${
          h(xp.fields.externalQuery)
        }</textarea></label>`
        : ''
    }`;
  }
  if (xp.section === 'transfers') {
    return `<h3>${
      xpLabel('迁移配置', 'Transfer configuration')
    }</h3><div class="xp-file-list"><button data-xp-file="transfers:current" aria-pressed="true"><strong>${
      h(xp.fields.transferName)
    }</strong><code>transfers/old.json</code><span>${
      xp.fields.oldTransfer ? xpLabel('保留在草稿', 'Retained in draft') : xpLabel('已从草稿删除', 'Removed from draft')
    }</span></button></div><h4>${xpLabel('编辑所选配置', 'Edit selected configuration')}</h4><div class="xp-grid">${
      xpField('transferName', '配置名称', 'Configuration name')
    }${xpField('transferConfig', '配置正文', 'Configuration text', true)}</div>`;
  }
  if (xp.section === 'save') {
    return `<h3>${xpLabel('保存差异', 'Save changes')}</h3><p role="status">${
      xp.savePreview
        ? xpLabel(
          '已准备本页示例差异；确认后仅更新本页基线，不写入磁盘。',
          'Example changes prepared; confirmation updates only the page baseline and does not write to disk.',
        )
        : xpLabel('请选择查看保存差异以准备预览。', 'Choose Review save changes to prepare a preview.')
    }</p>${
      xp.savePreview
        ? `<div class="xp-table"><table><thead><tr><th>${xpLabel('字段', 'Field')}</th><th>${
          xpLabel('原基线', 'Baseline')
        }</th><th>${xpLabel('共享草稿', 'Shared draft')}</th></tr></thead><tbody>${
          xpProjectKeys.filter(key => !xp.projectBaseline || xp.projectBaseline[key] !== xp.fields[key]).map(key =>
            `<tr><td>${h(key)}</td><td><pre>${h(xp.projectBaseline?.[key] || '')}</pre></td><td><pre>${
              h(xp.fields[key])
            }</pre></td></tr>`
          ).join('') || `<tr><td colspan="3">${xpLabel('无差异', 'No changes')}</td></tr>`
        }</tbody></table></div>`
          + (xp.projectBaseline?.query !== xp.fields.query
            ? `<details class="xp-diff"><summary>${
              xpLabel('逐行对比示例', 'Compare example lines')
            }</summary><div><pre>− ${h(xp.projectBaseline?.query || '')}</pre><pre>+ ${
              h(xp.fields.query)
            }</pre></div></details>`
            : '')
        : ''
    }${xpProjectConflicts()}`;
  }
  return `<h3>${xpLabel('遗留资源', 'Leftover resources')}</h3><div class="xp-file-list"><div><strong>${
    xpLabel('部分导出文件', 'Partial export file')
  }</strong><code>orders.csv.partial</code><span>${
    xpLabel('身份待核对', 'Identity requires verification')
  }</span></div></div><details><summary>${xpLabel('文件身份与摘要', 'File identity and summary')}</summary><p>${
    xpLabel('示例身份待核对；不是完整导出。', 'Example identity requires verification; it is not a complete export.')
  }</p></details><label class="xp-choice"><input type="checkbox" data-xp-confirm${xp.confirmed ? ' checked' : ''}> ${
    xpLabel('已核对身份，预览领取', 'Identity reviewed; preview claim')
  }</label><button data-xp-action="claim"${xp.confirmed ? '' : ' disabled'}>${
    xpLabel('预览领取（未发送）', 'Preview claim (not sent)')
  }</button>`;
}
function xpProjectConflicts() {
  return `<h4>${xpLabel('外部冲突', 'External conflicts')}</h4><p>${
    xpLabel(
      '逐项决定如何合并外部新增、修改和删除；接受后仅更新本页示例。',
      'Decide how to merge each external addition, modification and deletion; acceptance updates only this page example.',
    )
  }</p><div class="xp-grid"><label class="xp-field"><span>${
    xpLabel('本机查询草稿', 'Local query draft')
  }</span><textarea readonly rows="3">${h(xp.fields.query)}</textarea></label><label class="xp-field"><span>${
    xpLabel('外部查询内容', 'External query content')
  }</span><textarea readonly rows="3">SELECT id FROM orders;</textarea></label></div>${
    [
      [
        'added',
        '外部新增 queries/new.sql',
        'External addition queries/new.sql',
        '合并新增',
        'Merge addition',
        '忽略新增',
        'Ignore addition',
      ],
      [
        'modified',
        '外部修改 queries/orders.sql',
        'External modification queries/orders.sql',
        '采用外部内容',
        'Use external content',
        '保留本机草稿',
        'Keep local draft',
      ],
      [
        'deleted',
        '外部删除 transfers/old.json',
        'External deletion transfers/old.json',
        '接受删除',
        'Accept deletion',
        '保留本机配置',
        'Keep local configuration',
      ],
    ].map(([key, zh, en, a, b, c, d]) =>
      `<label class="xp-field"><span>${xpLabel(zh, en)}</span><select data-xp-conflict="${key}"><option value="">${
        xpLabel('请选择', 'Choose')
      }</option><option value="external"${xp.conflictDecisions[key] === 'external' ? ' selected' : ''}>${
        xpLabel(a, b)
      }</option><option value="local"${xp.conflictDecisions[key] === 'local' ? ' selected' : ''}>${
        xpLabel(c, d)
      }</option></select></label>`
    ).join('')
  }<button data-xp-action="accept-baseline"${Object.values(xp.conflictDecisions).every(Boolean) ? '' : ' disabled'}>${
    xpLabel('接受新基线（仅示例）', 'Accept new baseline (example only)')
  }</button>`;
}
function mcpField(key, zh, en) {
  return `<label class="xp-field"><span>${xpLabel(zh, en)}</span><input data-mcp-field="${key}" value="${
    h(mcpExample.draft[key] || '')
  }"></label>`;
}
function mcpIcon(name, zh, en, action, disabled = false) {
  const label = xpLabel(zh, en);
  return `<button class="mcp-icon-button" data-mcp-action="${action}" title="${label}" aria-label="${label}"${
    disabled ? ' disabled' : ''
  }>${icon(name)}</button>`;
}
function xpIcon(name, zh, en, action) {
  const label = xpLabel(zh, en);
  return `<button class="mcp-icon-button" data-xp-action="${action}" title="${label}" aria-label="${label}">${
    icon(name)
  }</button>`;
}
function mcpNavigation() {
  return `<div class="xp-tabs" role="tablist" aria-label="${xpLabel('项目工作台', 'Project workbench')}">${
    [
      ['mcp', 'MCP 项目', 'MCP projects'],
      ['files', '项目文件', 'Project files'],
    ].map(([id, zh, en]) =>
      `<button role="tab" data-mcp-action="view:${id}" aria-selected="${mcpExample.view === id}">${
        xpLabel(zh, en)
      }</button>`
    ).join('')
  }</div>`;
}
function mcpDialog() {
  const form = mcpExample.form;
  if (!form) return '';
  const draft = mcpExample.draft;
  const rootStep = form === 'project' && mcpExample.rootStep;
  const title = rootStep
    ? xpText('添加仓库目录', 'Add repository directory')
    : form === 'project'
    ? (mcpExample.editIndex < 0 ? xpText('新建项目', 'New project') : xpText('项目设置', 'Project settings'))
    : form === 'environment'
    ? (mcpExample.editIndex < 0 ? xpText('新增环境', 'Add environment') : xpText('编辑环境', 'Edit environment'))
    : (mcpExample.editIndex < 0 ? xpText('关联数据库', 'Associate database') : xpText('编辑关联', 'Edit association'));
  let fields = '';
  if (rootStep) fields = mcpField('rootPath', '仓库目录', 'Repository directory');
  else if (form === 'project') {
    fields = `<div class="xp-grid">${mcpField('name', '项目名称', 'Project name')}${
      mcpField('description', '项目说明', 'Description')
    }</div><div class="mcp-environment-heading"><strong>${
      xpLabel('仓库目录', 'Repository directories')
    }</strong><button data-mcp-action="root-add">${xpLabel('添加仓库', 'Add repository')}</button></div>${
      draft.roots.length
        ? draft.roots.map((root, index) =>
          `<div class="mcp-resource"><span>${h(root)}</span><button data-mcp-action="root-remove:${index}">${
            xpLabel('移除', 'Remove')
          }</button></div>`
        ).join('')
        : `<p>${xpLabel('尚未添加仓库目录。', 'No repository directories added.')}</p>`
    }`;
  } else if (form === 'environment') {
    fields = `<div class="xp-grid">${mcpField('name', '环境名称', 'Environment name')}${
      mcpField('type', '环境类型', 'Environment type')
    }${mcpField('description', '环境用途', 'Purpose')}</div>`;
  } else {
    const profile = mcpProfiles.find(item => item.name === draft.profile);
    const key = draft.type === 'redis' ? 'redisDb' : 'database';
    const current = draft[key] || '';
    const options = mcpProfiles.map(item =>
      `<option value="${h(item.name)}"${draft.profile === item.name ? ' selected' : ''}>${
        h(item.name)
      } · ${item.type.toUpperCase()}</option>`
    ).join('');
    const unavailableProfile = draft.profile && !profile
      ? `<option value="${h(draft.profile)}" selected disabled>${h(draft.profile)} · ${
        xpLabel('不可用', 'Unavailable')
      }</option>`
      : '';
    const unavailableDatabase = current && !profile?.databases.includes(current)
      ? `<option value="${h(current)}" selected disabled>${h(current)} · ${xpLabel('不可用', 'Unavailable')}</option>`
      : '';
    const databases =
      profile?.databases.map(value =>
        `<option value="${h(value)}"${current === value ? ' selected' : ''}>${draft.type === 'redis' ? 'DB ' : ''}${
          h(value)
        }</option>`
      ).join('') || '';
    const state = mcpExample.catalogState || 'ready';
    fields = `<div class="xp-grid"><label class="xp-field"><span>${
      xpLabel('已有实例', 'Existing instance')
    }</span><select data-mcp-field="profile"><option value="">${
      xpLabel('请选择实例', 'Select an instance')
    }</option>${unavailableProfile}${options}</select></label><label class="xp-field"><span>${
      draft.type === 'redis' ? 'Redis DB' : xpLabel('数据库', 'Database')
    }</span><select data-mcp-field="${key}"${!profile || state !== 'ready' ? ' disabled' : ''}><option value="">${
      xpLabel('请选择数据库', 'Select a database')
    }</option>${unavailableDatabase}${databases}</select></label>${
      mcpField('description', '资源用途', 'Purpose')
    }</div><label class="xp-field"><span>${
      xpLabel('示例目录状态', 'Example catalog state')
    }</span><select data-mcp-catalog-state><option value="ready"${state === 'ready' ? ' selected' : ''}>${
      xpLabel('示例数据', 'Example data')
    }</option><option value="loading"${state === 'loading' ? ' selected' : ''}>${
      xpLabel('加载中', 'Loading')
    }</option><option value="failed"${state === 'failed' ? ' selected' : ''}>${
      xpLabel('加载失败', 'Failed')
    }</option></select></label>${
      state === 'loading'
        ? `<p role="status">${xpLabel('正在加载数据库列表（示例）。', 'Loading database list (example).')}</p>`
        : state === 'failed'
        ? `<div class="mcp-feedback"><p role="alert">${
          xpLabel('数据库列表加载失败（示例）。', 'Database list failed to load (example).')
        }</p>${mcpIcon('refresh', '重试', 'Retry', 'database-retry', !profile)}</div>`
        : ''
    }`;
  }
  return `<dialog class="mcp-dialog" aria-labelledby="mcp-dialog-title"><div class="mcp-dialog-header"><h3 id="mcp-dialog-title">${
    h(title)
  }</h3></div><div class="mcp-dialog-body">${fields}${
    mcpExample.error ? `<p role="alert">${h(mcpExample.error)}</p>` : ''
  }<p class="mcp-notice">${
    xpLabel('示例操作未发送，不读取或写入文件。', 'Example actions are not sent; no files are read or written.')
  }</p></div><div class="mcp-dialog-footer"><button data-mcp-action="${rootStep ? 'root-cancel' : 'cancel'}">${
    xpLabel('取消', 'Cancel')
  }</button><button class="primary" data-mcp-action="${rootStep ? 'root-save' : 'save'}">${
    rootStep ? xpLabel('添加', 'Add') : xpLabel('保存示例', 'Save example')
  }</button></div></dialog>`;
}
function renderMcpProject(area) {
  const project = mcpExample.projects[mcpExample.selected];
  const environment = project?.environments[mcpExample.environment];
  let content = '';
  if (!project || xp.scenario === 'empty') {
    content = `<p>${xpLabel('暂无项目，请新建项目。', 'No projects. Create a project to begin.')}</p>`;
  } else if (xp.scenario === 'loading') {
    content = `<p role="status">${xpLabel('正在加载项目示例…', 'Loading project example…')}</p>`;
  } else {
    content = `<div class="mcp-project-heading"><div><h3>${h(project.name)}</h3><div class="mcp-roots">${
      project.roots.length
        ? project.roots.map(root => `<div>${icon('folder')}<code>${h(root)}</code></div>`).join('')
        : `<span>${xpLabel('尚未添加仓库目录', 'No repository directories')}</span>`
    }</div></div>${
      mcpIcon('gear', '项目设置', 'Project settings', 'project-settings')
    }</div><div class="mcp-tabs-row"><div class="xp-tabs mcp-environment-tabs" role="tablist" aria-label="${
      xpLabel('环境', 'Environments')
    }">${
      project.environments.map((item, index) =>
        `<button role="tab" data-mcp-action="environment:${index}" aria-selected="${
          index === mcpExample.environment
        }">${h(item.name)}</button>`
      ).join('')
    }</div>${mcpIcon('plus', '新增环境', 'Add environment', 'environment-add')}</div>${
      environment
        ? `<div class="mcp-environment-summary"><div><span>${
          h(environment.description || xpText('暂无用途说明', 'No purpose specified'))
        }</span>${
          environment.default ? `<span class="mcp-default">${xpLabel('默认', 'Default')}</span>` : ''
        }</div><div class="xp-actions">${mcpIcon('edit', '编辑环境', 'Edit environment', 'environment-edit')}${
          mcpIcon('check', '设为默认', 'Set default', 'environment-default', environment.default)
        }${
          mcpIcon('trash', '移除环境', 'Remove environment', 'environment-remove')
        }</div></div><div class="mcp-database-heading"><h3>${
          xpLabel('已关联数据库', 'Associated databases')
        } <span>${environment.resources.length}</span></h3>${
          mcpIcon('plus', '关联数据库', 'Associate database', 'resource-add')
        }</div>${
          environment.resources.length
            ? `<div class="mcp-resource-list"><div class="mcp-resource-columns" aria-hidden="true"><span>${
              xpLabel('实例', 'Instance')
            }</span><span>${xpLabel('数据库', 'Database')}</span><span>${xpLabel('用途', 'Purpose')}</span><span>${
              xpLabel('操作', 'Actions')
            }</span></div>${
              environment.resources.map((resource, index) =>
                `<div class="mcp-resource-row"><div class="mcp-instance">${icon('database')}<div><strong>${
                  h(resource.profile)
                }</strong>${
                  resource.invalid
                    ? `<div class="mcp-invalid">${xpLabel('连接已失效', 'Connection unavailable')}</div>`
                    : ''
                }</div></div><code>${
                  resource.type === 'redis' ? `Redis DB ${h(resource.redisDb)}` : `MySQL / ${h(resource.database)}`
                }</code><span class="mcp-purpose">${
                  h(resource.description || '—')
                }</span><div class="xp-actions mcp-resource-actions">${
                  mcpIcon('edit', '编辑', 'Edit', `resource-edit:${index}`)
                }${mcpIcon('trash', '移除', 'Remove', `resource-remove:${index}`)}</div></div>`
              ).join('')
            }</div>`
            : `<p>${xpLabel('当前环境尚未关联数据库。', 'No databases associated with this environment.')}</p>`
        }`
        : `<p>${xpLabel('暂无环境，请新增环境。', 'No environments. Add an environment.')}</p>`
    }`;
  }
  area.innerHTML = `<section class="xp-workbench project-workbench"><header class="xp-header"><div><h2>${
    xpLabel('项目', 'Project')
  }</h2><p>${
    xpLabel('交互预览 · 示例数据 · 所有动作未发送', 'Interactive preview · example data · all actions not sent')
  }</p></div><button data-xp-action="close">${
    xpLabel('返回工作区', 'Return to workspace')
  }</button></header>${mcpNavigation()}<div class="mcp-layout"><aside class="mcp-project-list"><div class="mcp-list-heading"><strong>${
    xpLabel('项目', 'Projects')
  }</strong><div>${mcpIcon('plus', '新建项目', 'New project', 'project-add')}${
    mcpIcon('repeat', '刷新项目', 'Refresh projects', 'project-refresh')
  }</div></div>${
    mcpExample.projects.map((item, index) =>
      `<button class="mcp-project-row" data-mcp-action="project:${index}" aria-pressed="${
        index === mcpExample.selected
      }">${h(item.name)}</button>`
    ).join('')
  }</aside><div class="xp-body mcp-detail">${content}<p role="status" class="mcp-notice">${
    h(
      xp.message
        || xpText('示例操作未发送，不读取或写入文件。', 'Example actions are not sent; no files are read or written.'),
    )
  }</p></div></div>${mcpDialog()}</section>`;
  area.querySelector('.mcp-dialog')?.showModal();
}
function renderExchangeProject() {
  if (!xp.open) return;
  const area = document.querySelector('#app .workarea');
  if (!area) return;
  const exchange = xp.open === 'exchange';
  if (!exchange && mcpExample.view === 'mcp') return renderMcpProject(area);
  const tabs = exchange
    ? [
      ['sources', '来源与目标', 'Source and target'],
      ['objects', '对象与映射', 'Objects and mappings'],
      ['policy', '策略', 'Policy'],
      ['preview', '预览', 'Preview'],
      ['report', '报告', 'Report'],
    ]
    : [
      ['overview', '概览', 'Overview'],
      ['bindings', '本机绑定', 'Local bindings'],
      ['queries', '查询', 'Queries'],
      ['transfers', '迁移配置', 'Transfers'],
      ['save', '保存差异', 'Save changes'],
      ['recovery', '恢复', 'Recovery'],
    ];
  const readOnly = xp.scenario === 'readonly';
  area.innerHTML = `<section class="xp-workbench ${xp.open}-workbench" aria-label="${
    exchange ? xpLabel('数据交换与迁移', 'Data exchange and migration') : xpLabel('项目', 'Project')
  }"><header class="xp-header"><div><h2>${
    exchange ? xpLabel('数据交换与迁移', 'Data exchange and migration') : xpLabel('项目', 'Project')
  }</h2><p>${
    xpLabel('交互预览 · 示例数据 · 所有动作未发送', 'Interactive preview · example data · all actions not sent')
  }</p></div><button data-xp-action="close">${xpLabel('返回工作区', 'Return to workspace')}</button></header>${
    exchange ? '' : mcpNavigation()
  }<div class="xp-summary"><strong>${xpLabel('目标摘要', 'Target summary')}</strong><div class="xp-summary-target">${
    exchange
      ? `<span><b>${xpLabel('来源', 'Source')}</b> <code data-xp-summary="source">${
        h(
          ['import', 'sql_restore'].includes(xp.mode)
            ? xp.files.join(' · ') || '—'
            : [xp.endpoints.source.connection, xp.endpoints.source.database].filter(Boolean).join(' / ') || '—',
        )
      }</code></span><span><b>${xpLabel('目标', 'Target')}</b> <code data-xp-summary="target">${
        h(
          ['export', 'sql_dump'].includes(xp.mode)
            ? xp.fields.target
            : [xp.endpoints.target.connection, xp.endpoints.target.database].filter(Boolean).join(' / ') || '—',
        )
      }</code></span><span>${h(xp.provider.toUpperCase())}</span>`
      : `<span>${h(xp.fields.projectName)}</span><span><b>${xpLabel('项目目录', 'Project directory')}</b> <code>${
        h(xp.fields.projectPath)
      }</code></span>`
  }</div><span>${readOnly ? xpLabel('只读示例', 'Read-only example') : xpLabel('未发送', 'Not sent')}</span></div>${
    xpTabs(tabs)
  }<div class="xp-body ${xp.open}-body">${
    readOnly
      ? `<p role="status">${
        xpLabel(
          '只读连接：写入操作不可用；仍可查看配置。',
          'Read-only connection: write actions are disabled; configuration remains visible.',
        )
      }</p>`
      : ''
  }${
    exchange ? xpExchangeBody() : xpProjectBody()
  }</div><footer class="xp-footer ${xp.open}-footer"><span role="status">${
    h(xp.message || xpText('示例 · 未发送', 'Example · not sent'))
  }</span><div class="xp-actions">${
    exchange
      ? xp.scenario === 'running'
        ? `<button data-xp-action="cancel">${xpLabel('预览取消', 'Preview cancellation')}</button>`
        : xp.section === 'report'
        ? `<button data-xp-action="resume">${
          xpLabel('预览可续作项（未发送）', 'Preview resumable items (not sent)')
        }</button>`
        : xp.section === 'preview' && xp.prepared
        ? `<button data-xp-action="discard-plan">${
          xpLabel('丢弃计划', 'Discard plan')
        }</button><button class="primary" data-xp-action="execute"${readOnly || !xp.confirmed ? ' disabled' : ''}>${
          xpLabel('预览执行（未发送）', 'Preview execution (not sent)')
        }</button>`
        : `<button class="primary" data-xp-action="prepare"${readOnly ? ' disabled' : ''}>${
          xpLabel('准备预览', 'Prepare preview')
        }</button>`
      : xp.section === 'save' && xp.savePreview
      ? `<button class="primary" data-xp-action="save"${readOnly ? ' disabled' : ''}>${
        xpLabel('确认示例保存（未发送）', 'Confirm example save (not sent)')
      }</button>`
      : `<button class="primary" data-xp-action="save-preview">${
        xpLabel('查看保存差异', 'Review save changes')
      }</button>`
  }</div></footer>${
    xp.pendingLeave
      ? `<div role="alertdialog" aria-label="${xpLabel('共享草稿未确认', 'Unconfirmed shared draft')}"><p>${
        xpLabel(
          '共享草稿尚未确认。保留在本页、丢弃，或返回查看保存差异。均不会写入磁盘。',
          'The shared draft is unconfirmed. Keep it on this page, discard it, or return to review save changes. Nothing is written to disk.',
        )
      }</p><div class="xp-actions"><button data-xp-action="leave-keep">${
        xpLabel('保留草稿并离开', 'Keep draft and leave')
      }</button><button data-xp-action="leave-discard">${
        xpLabel('丢弃草稿并离开', 'Discard draft and leave')
      }</button><button data-xp-action="leave-cancel">${xpLabel('返回草稿', 'Return to draft')}</button></div></div>`
      : ''
  }</section>`;
}
function xpOpen(kind, section = '') {
  xp.open = kind;
  xp.section = section || (kind === 'exchange' ? 'sources' : 'overview');
  if (kind === 'project') {
    mcpExample.view = !section || section === 'local' || ['empty', 'loading'].includes(xp.scenario) ? 'mcp' : 'files';
    mcpExample.form = '';
  }
  xp.message = '';
  state.overlay = null;
  state.menu = null;
  render();
}
document.addEventListener('cancel', event => {
  if (!event.target.matches('.mcp-dialog')) return;
  event.preventDefault();
  if (mcpExample.rootStep) {
    mcpExample.rootStep = false;
    mcpExample.error = '';
  } else {
    mcpExample.form = '';
    mcpExample.error = '';
  }
  renderExchangeProject();
  if (!mcpExample.form && mcpExample.opener) {
    document.querySelector(`[data-mcp-action="${mcpExample.opener}"]`)?.focus();
  }
}, true);
window.addEventListener('click', event => {
  const mcpButton = event.target.closest('.project-workbench [data-mcp-action]');
  if (mcpButton && !mcpButton.disabled) {
    event.stopImmediatePropagation();
    const [action, argument] = mcpButton.dataset.mcpAction.split(':');
    const index = Number(argument);
    const project = mcpExample.projects[mcpExample.selected];
    const environment = project?.environments[mcpExample.environment];
    if (action === 'view') {
      mcpExample.view = argument;
      mcpExample.form = '';
      xp.section = 'overview';
    }
    if (action === 'project') {
      mcpExample.selected = index;
      mcpExample.environment = 0;
      mcpExample.form = '';
      xp.scenario = 'default';
    }
    if (action === 'environment') {
      mcpExample.environment = index;
      mcpExample.form = '';
    }
    if (action === 'cancel') {
      mcpExample.form = '';
      mcpExample.error = '';
    }
    if (
      ['project-add', 'project-settings', 'environment-add', 'environment-edit', 'resource-add', 'resource-edit']
        .includes(action)
    ) mcpExample.opener = mcpButton.dataset.mcpAction;
    if (action === 'project-add' || action === 'project-settings') {
      mcpExample.form = 'project';
      mcpExample.editIndex = action === 'project-add' ? -1 : mcpExample.selected;
      mcpExample.rootStep = false;
      mcpExample.draft = action === 'project-add'
        ? { name: '', description: '', roots: [] }
        : { name: project.name, description: project.description, roots: [...project.roots] };
    }
    if (action === 'root-add') {
      mcpExample.rootStep = true;
      mcpExample.draft.rootPath = '';
      mcpExample.error = '';
    }
    if (action === 'root-cancel') {
      mcpExample.rootStep = false;
      mcpExample.error = '';
    }
    if (action === 'root-remove') mcpExample.draft.roots.splice(index, 1);
    if (action === 'root-save') {
      const path = mcpExample.draft.rootPath.trim();
      if (!path) mcpExample.error = xpText('请输入仓库目录。', 'Enter a repository directory.');
      else if (mcpExample.draft.roots.includes(path)) {
        mcpExample.error = xpText('该目录已添加。', 'This directory has already been added.');
      } else {
        mcpExample.draft.roots.push(path);
        mcpExample.rootStep = false;
        mcpExample.error = '';
      }
    }
    if (action === 'environment-add' || action === 'environment-edit') {
      mcpExample.form = 'environment';
      mcpExample.editIndex = action === 'environment-add' ? -1 : mcpExample.environment;
      mcpExample.draft = action === 'environment-add' ? { name: '', type: 'dev', description: '' } : { ...environment };
    }
    if (action === 'resource-add' || action === 'resource-edit') {
      mcpExample.form = 'resource';
      mcpExample.editIndex = action === 'resource-add' ? -1 : index;
      const existing = environment.resources[index];
      mcpExample.draft = action === 'resource-add'
        ? { profile: '', type: 'mysql', database: '', redisDb: '', description: '' }
        : { ...existing };
      mcpExample.catalogState = 'ready';
    }
    if (action === 'database-retry') {
      mcpExample.catalogState = 'ready';
      mcpExample.error = '';
    }
    if (action === 'environment-default') project.environments.forEach(item => item.default = item === environment);
    if (action === 'environment-remove') {
      project.environments.splice(mcpExample.environment, 1);
      mcpExample.environment = 0;
    }
    if (action === 'resource-remove') environment.resources.splice(index, 1);
    if (action === 'save') {
      const draft = mcpExample.draft;
      const profile = mcpProfiles.find(item => item.name === draft.profile);
      const required = mcpExample.form === 'resource'
        ? mcpExample.catalogState === 'ready'
          && profile?.databases.includes(draft.type === 'redis' ? draft.redisDb : draft.database)
        : draft.name?.trim();
      const duplicate = mcpExample.form === 'resource'
        && environment.resources.some((item, index) =>
          index !== mcpExample.editIndex && item.profile === draft.profile && item.type === draft.type
          && (draft.type === 'redis' ? item.redisDb === draft.redisDb : item.database === draft.database)
        );
      if (!required || duplicate) {
        mcpExample.error = duplicate
          ? xpText(
            '当前环境已关联该实例和数据库。',
            'This instance and database are already associated with this environment.',
          )
          : xpText(
            '请选择可用实例和数据库，或填写必填项。',
            'Select an available instance and database, or complete required fields.',
          );
      } else {
        if (mcpExample.form === 'project') {
          const value = { name: draft.name, description: draft.description, roots: [...draft.roots] };
          if (mcpExample.editIndex < 0) {
            mcpExample.projects.push({ ...value, environments: [] });
            mcpExample.selected = mcpExample.projects.length - 1;
            mcpExample.environment = 0;
          } else Object.assign(project, value);
        }
        if (mcpExample.form === 'environment') {
          const value = { name: draft.name, type: draft.type, description: draft.description };
          if (mcpExample.editIndex < 0) {
            project.environments.push({ ...value, default: false, resources: [] });
            mcpExample.environment = project.environments.length - 1;
          } else Object.assign(environment, value);
        }
        if (mcpExample.form === 'resource') {
          const value = {
            profile: draft.profile,
            type: draft.type,
            description: draft.description,
            ...(draft.type === 'redis' ? { redisDb: draft.redisDb } : { database: draft.database }),
          };
          if (mcpExample.editIndex < 0) environment.resources.push(value);
          else environment.resources[mcpExample.editIndex] = value;
        }
        mcpExample.form = '';
        mcpExample.error = '';
        xp.scenario = 'default';
      }
    }
    xp.message = xpText(
      '本页示例，未发送请求，也未读写文件。',
      'Page example; no request sent and no files read or written.',
    );
    renderExchangeProject();
    if (!mcpExample.form && ['save', 'cancel'].includes(action) && mcpExample.opener) {
      document.querySelector(`[data-mcp-action="${mcpExample.opener}"]`)?.focus();
    }
    return;
  }
  const open = event.target.closest('[data-workbench-open]');
  const resource = event.target.closest('[data-action="import"], [data-action="export"], [data-action="transfer"]');
  const navigation = open || resource
    || event.target.closest(
      '[data-provider], [data-tab], [data-open-table], [data-action="new-query"], [data-action="key-browser"]',
    );
  if (navigation && xpGuardLeave(() => navigation.click())) {
    event.preventDefault();
    event.stopImmediatePropagation();
    return;
  }
  if (open || resource && previewSource === 'direct') {
    xp.scenario = 'default';
    xp.prepared = false;
    xp.confirmed = false;
    event.stopImmediatePropagation();
    if (resource) {
      xp.mode = resource.dataset.action === 'transfer' ? 'copy' : resource.dataset.action;
      xp.provider = state.provider || 'mysql';
      const target = xp.provider === 'mysql'
        ? `${state.database}.${state.table}`
        : `DB ${state.redisDb} / ${state.key}`;
      if (xp.mode === 'import') {
        xp.fields.source = 'orders.csv';
        xp.fields.target = target;
      } else {
        xp.fields.source = target;
        xp.fields.target = xp.mode === 'export'
          ? 'orders.csv'
          : xpText('目标连接（示例）', 'Target connection (example)');
      }
    }
    xpOpen(open?.dataset.workbenchOpen || 'exchange');
    return;
  }
  if (
    xp.open
    && event.target.closest(
      '[data-provider], [data-tab], [data-open-table], [data-action="new-query"], [data-action="key-browser"]',
    )
  ) xp.open = '';
  const file = event.target.closest('[data-xp-file]');
  if (file) {
    event.stopImmediatePropagation();
    const [section, selection] = file.dataset.xpFile.split(':');
    xp.fileSelection[section] = selection;
    renderExchangeProject();
    document.querySelector(`[data-xp-file="${section}:${selection}"]`)?.focus();
    return;
  }
  const previewTab = event.target.closest('[data-xp-preview]');
  if (previewTab) {
    event.stopImmediatePropagation();
    xp.previewTab = previewTab.dataset.xpPreview;
    renderExchangeProject();
    document.querySelector(`[data-xp-preview="${xp.previewTab}"]`)?.focus();
    return;
  }
  const section = event.target.closest('[data-xp-section]');
  if (section) {
    event.stopImmediatePropagation();
    xp.section = section.dataset.xpSection;
    renderExchangeProject();
    document.querySelector(`[data-xp-section="${xp.section}"]`)?.focus();
    return;
  }
  const button = event.target.closest('[data-xp-action]');
  if (!button || button.disabled) return;
  event.stopImmediatePropagation();
  const action = button.dataset.xpAction;
  if (action.startsWith('mapping-object:')) {
    if (xp.objects[xp.activeObject]) xp.objects[xp.activeObject].mappings = xp.mappings;
    xp.activeObject = Number(action.split(':')[1]);
    xp.mappings = xp.objects[xp.activeObject].mappings || [{ source: '', target: '' }];
    renderExchangeProject();
    return;
  }
  if (action === 'catalog-retry') {
    xp.catalogState = 'ready';
    renderExchangeProject();
    return;
  }
  if (action === 'new-target' || action === 'new-target-cancel') {
    xp.newTargetStep = action === 'new-target';
    xp.newTargetName = '';
    renderExchangeProject();
    return;
  }
  if (action === 'new-target-save') {
    const endpoint = xp.endpoints.target;
    const database = xpCatalog[xp.provider][endpoint.connection]?.[endpoint.database];
    const name = xp.newTargetName.trim();
    if (!database || !name || Object.hasOwn(database, name)) {
      xp.message = xpText(
        '请选择目标数据库并填写未使用的表名。',
        'Select a target database and enter an unused table name.',
      );
      renderExchangeProject();
      return;
    }
    database[name] = [...xpFileColumns];
    xp.newTargetStep = false;
    xp.newTargetName = '';
    xp.message = xpText(
      '仅添加本页示例，未创建数据库对象。',
      'Added to this page example only; no database object was created.',
    );
    renderExchangeProject();
    return;
  }
  if (action.startsWith('collection-')) {
    const [command, key, rawIndex] = action.split(':');
    if (command === 'collection-add') {
      xp[key].push(
        key === 'objects'
          ? { source: '', target: '', checked: true }
          : key === 'mappings'
          ? { source: '', target: '' }
          : '',
      );
    } else xp[key].splice(Number(rawIndex), 1);
    if (key === 'objects' && command === 'collection-remove') {
      xp.activeObject = 0;
      xp.mappings = xp.objects[0]?.mappings || [{ source: '', target: '' }];
    }
    if (key === 'files') {
      xp.objects.forEach(item => {
        item.source = '';
        (item.mappings || []).forEach(mapping => {
          mapping.source = '';
        });
      });
      xp.mappings.forEach(item => {
        item.source = '';
      });
    }
    xp.prepared = false;
    xp.confirmed = false;
    renderExchangeProject();
    const selector = command === 'collection-add'
      ? key === 'objects'
        ? `[data-xp-object="${xp.objects.length - 1}:source"]`
        : key === 'mappings'
        ? `[data-xp-mapping="${xp.mappings.length - 1}:source"]`
        : `[data-xp-collection="${key}:${xp[key].length - 1}"]`
      : `[data-xp-action="collection-add:${key}"]`;
    document.querySelector(selector)?.focus();
    return;
  }
  if (action.startsWith('leave-')) {
    xpFinishLeave(action.slice(6));
    return;
  }
  if (
    ['create', 'open', 'archive-import'].includes(action) && xpGuardLeave(() => {
      document.querySelector(`[data-xp-action="${action}"]`)?.click();
    })
  ) return;
  if (action === 'execute' && (!xp.prepared || !xp.confirmed || xp.scenario === 'readonly')) return;
  if (action === 'close') {
    if (
      xpGuardLeave(() => {
        xp.open = '';
        render();
      })
    ) return;
    xp.open = '';
    render();
    return;
  }
  if (action === 'discard-plan') {
    xp.prepared = false;
    xp.confirmed = false;
  }
  if (action === 'prepare') {
    xp.prepared = true;
    xp.confirmed = false;
    xp.section = 'preview';
  }
  if (action === 'execute') {
    xp.section = 'report';
    xp.scenario = 'default';
  }
  if (action === 'cancel') {
    xp.section = 'report';
    xp.scenario = 'default';
  }
  if (action === 'create' || action === 'open' || action === 'archive-import') {
    xp.projectActive = true;
    xp.scenario = 'default';
    if (!xp.projectDirty) xp.projectBaseline = xpSnapshot();
    if (action !== 'open') xp.bindingConfirmed = false;
    xp.section = action === 'archive-import' ? 'bindings' : 'overview';
    xp.message = action === 'create'
      ? xpText(
        '已创建项目示例；未创建目录或发送请求。',
        'Project example created; no directory created or request sent.',
      )
      : xpText('已打开项目示例；未读取磁盘或发送请求。', 'Project example opened; no disk read or request sent.');
    renderExchangeProject();
    return;
  }
  if (action === 'save-preview') {
    xp.savePreview = true;
    xp.section = 'save';
  }
  if (action === 'save') {
    if (!xp.savePreview) return;
    xp.projectBaseline = xpSnapshot();
    xp.projectDirty = false;
    xp.savePreview = false;
    xp.section = 'save';
    xp.message = xpText(
      '示例保存已确认，页面草稿已清除；未写入磁盘。',
      'Example save confirmed and page draft cleared; nothing written to disk.',
    );
    renderExchangeProject();
    return;
  }
  if (action === 'accept-baseline') {
    if (!Object.values(xp.conflictDecisions).every(Boolean)) return;
    if (xp.conflictDecisions.modified === 'external') xp.fields.query = 'SELECT id FROM orders;';
    xp.fields.externalQuery = xp.conflictDecisions.added === 'external' ? 'SELECT * FROM new_orders;' : '';
    if (xp.conflictDecisions.deleted === 'external') xp.fields.oldTransfer = '';
    else if (!xp.fields.oldTransfer) xp.fields.oldTransfer = '{ "mode": "copy" }';
    xp.projectBaseline = {
      ...xp.projectBaseline,
      query: 'SELECT id FROM orders;',
      externalQuery: 'SELECT * FROM new_orders;',
      oldTransfer: '',
    };
    xp.projectDirty = xpProjectKeys.some(key => xp.fields[key] !== xp.projectBaseline[key]);
    xp.savePreview = false;
    xp.message = xpText(
      '已接受示例基线及各项决定；合并结果仅保留在本页，未写入磁盘。',
      'Example baseline and decisions accepted; the merged result remains on this page, nothing written to disk.',
    );
    renderExchangeProject();
    return;
  }
  xp.message = xpText('仅预览，未发送请求；没有执行或保存。', 'Preview only; no request sent, executed or saved.');
  renderExchangeProject();
}, true);
document.addEventListener('input', event => {
  if (event.target.hasAttribute('data-xp-document-ratio')) {
    xp.documentRatio = Number(event.target.value);
    event.target.closest('.xp-document-preview').querySelector('.xp-document-split').style.gridTemplateColumns =
      `minmax(0, ${xp.documentRatio}fr) minmax(0, ${100 - xp.documentRatio}fr)`;
    return;
  }
  if (event.target.hasAttribute('data-xp-new-target')) {
    xp.newTargetName = event.target.value;
    return;
  }
  const collection = event.target.dataset.xpCollection;
  if (collection) {
    const [key, rawIndex] = collection.split(':');
    const index = Number(rawIndex);
    if (key === 'objects') xp.objects[index].name = event.target.value;
    else xp[key][index] = event.target.value;
    if (key === 'files') {
      xp.objects.forEach(item => {
        item.source = '';
        (item.mappings || []).forEach(mapping => {
          mapping.source = '';
        });
      });
      xp.mappings.forEach(item => {
        item.source = '';
      });
    }
    if (key === 'files') document.querySelector('[data-xp-summary="source"]').textContent = xp.files.join(' · ') || '—';
    xp.prepared = false;
    xp.confirmed = false;
    return;
  }
  if (event.target.hasAttribute('data-xp-collection-check')) {
    xp.objects[Number(event.target.dataset.xpCollectionCheck)].checked = event.target.checked;
    xp.prepared = false;
    xp.confirmed = false;
    return;
  }
  if (event.target.dataset.mcpField) {
    mcpExample.draft[event.target.dataset.mcpField] = event.target.value;
    return;
  }
  const field = event.target.dataset.xpField;
  if (!field) return;
  xp.fields[field] = event.target.type === 'checkbox' ? event.target.checked : event.target.value;
  if (field === 'binding' || field === 'alias') {
    xp.bindingConfirmed = false;
    const confirmation = document.querySelector('[data-xp-binding]');
    if (confirmation) confirmation.checked = false;
  }
  if (xp.open === 'exchange' && (field === 'source' || field === 'target')) {
    const summary = document.querySelector(`[data-xp-summary="${field}"]`);
    if (summary) summary.textContent = xp.fields[field];
  }
  if (xp.open === 'project') {
    const summary = document.querySelector('.xp-summary-target');
    if (summary) summary.textContent = `${xp.fields.projectName} · ${xp.fields.projectPath}`;
  }
  xp.prepared = false;
  xp.confirmed = false;
  if (xp.open === 'project' && xpProjectKeys.includes(field)) {
    xp.projectDirty = true;
    xp.savePreview = false;
    document.querySelector('[data-xp-action="save"]')?.setAttribute('disabled', '');
  }
  document.querySelector('[data-xp-action="execute"]')?.setAttribute('disabled', '');
}, true);
document.addEventListener('change', event => {
  const target = event.target;
  if (target.hasAttribute('data-xp-catalog')) {
    xp.catalogState = target.value;
    renderExchangeProject();
    return;
  }
  if (target.dataset.xpColumns) {
    xp.fields[target.dataset.xpColumns] = Array.from(target.selectedOptions, option => option.value).join(', ');
    xp.prepared = false;
    return;
  }
  if (target.dataset.xpEndpoint) {
    const [side, field] = target.dataset.xpEndpoint.split(':');
    xp.endpoints[side][field] = target.value;
    if (field === 'connection') xp.endpoints[side].database = '';
    xp.objects.forEach(item => {
      item[side] = '';
      (item.mappings || []).forEach(mapping => {
        mapping[side] = '';
      });
    });
    xp.mappings.forEach(item => {
      item[side] = '';
    });
    xp.prepared = false;
    renderExchangeProject();
    return;
  }
  if (target.dataset.xpObject || target.dataset.xpMapping) {
    const isObject = !!target.dataset.xpObject;
    const [index, side] = (target.dataset.xpObject || target.dataset.xpMapping).split(':');
    (isObject ? xp.objects : xp.mappings)[Number(index)][side] = target.value;
    if (isObject) {
      const object = xp.objects[Number(index)];
      const mappings = Number(index) === xp.activeObject ? xp.mappings : object.mappings || [];
      mappings.forEach(item => {
        item[side] = '';
      });
    }
    xp.prepared = false;
    renderExchangeProject();
    return;
  }
  if (target.hasAttribute('data-mcp-catalog-state')) {
    mcpExample.catalogState = target.value;
    renderExchangeProject();
    document.querySelector('[data-mcp-catalog-state]')?.focus();
    return;
  }
  if (target.dataset.mcpField === 'profile') {
    const profile = mcpProfiles.find(item => item.name === target.value);
    mcpExample.draft.profile = target.value;
    mcpExample.draft.type = profile?.type || 'mysql';
    mcpExample.draft.database = '';
    mcpExample.draft.redisDb = '';
    mcpExample.error = '';
    renderExchangeProject();
    document.querySelector('[data-mcp-field="profile"]')?.focus();
    return;
  }
  if (target.dataset.xpConflict) {
    xp.conflictDecisions[target.dataset.xpConflict] = target.value;
    renderExchangeProject();
    document.querySelector(`[data-xp-conflict="${target.dataset.xpConflict}"]`)?.focus();
    return;
  }
  if (['preview-state', 'preview-source'].includes(target.id)) {
    const value = target.value;
    if (
      xpGuardLeave(() => {
        target.value = value;
        target.dispatchEvent(new Event('change', { bubbles: true }));
      })
    ) {
      target.value = xp.previousSelect;
      event.stopImmediatePropagation();
      return;
    }
  }
  if (target.id === 'preview-state') {
    const value = target.value;
    if (value.startsWith('exchange-') || value.startsWith('project-')) {
      event.stopImmediatePropagation();
      previewSource = 'direct';
      document.getElementById('preview-source').value = 'direct';
      const [kind, part] = value.split('-');
      xp.scenario = ['running', 'cancelled', 'unknown', 'failed', 'readonly', 'loading', 'empty'].includes(part)
        ? part
        : 'default';
      xp.prepared = part === 'preview';
      xp.confirmed = false;
      xpOpen(
        kind,
        ['readonly', 'loading', 'empty'].includes(part)
          ? (kind === 'exchange' ? 'sources' : 'overview')
          : kind === 'exchange' && ['running', 'cancelled', 'unknown', 'failed'].includes(part)
          ? 'report'
          : part === 'conflict'
          ? 'save'
          : part,
      );
      return;
    }
    xp.open = '';
  }
  if (target.id === 'preview-source') xp.open = '';
  if (target.hasAttribute('data-xp-mode')) {
    xp.mode = target.value;
    xp.objects = [{ source: '', target: '', checked: true }];
    xp.activeObject = 0;
    xp.mappings = [{ source: '', target: '' }];
    xp.prepared = false;
    renderExchangeProject();
  }
  if (target.hasAttribute('data-xp-provider')) {
    xp.provider = target.value;
    xp.endpoints = { source: { connection: '', database: '' }, target: { connection: '', database: '' } };
    xp.objects = [{ source: '', target: '', checked: true }];
    xp.activeObject = 0;
    xp.mappings = [{ source: '', target: '' }];
    if (xp.provider === 'redis' && xp.mode.startsWith('sql_')) xp.mode = 'import';
    xp.prepared = false;
    renderExchangeProject();
  }
  if (target.dataset.xpField === 'format') renderExchangeProject();
  if (target.dataset.xpChoice) {
    xp.selected[target.dataset.xpChoice] = target.checked;
    xp.prepared = false;
    xp.confirmed = false;
    renderExchangeProject();
    document.querySelector(`[data-xp-choice="${target.dataset.xpChoice}"]`)?.focus();
  }
  if (target.hasAttribute('data-xp-confirm')) {
    xp.confirmed = target.checked;
    renderExchangeProject();
    document.querySelector('[data-xp-confirm]')?.focus();
  }
  if (target.hasAttribute('data-xp-binding')) {
    xp.bindingConfirmed = target.checked;
    renderExchangeProject();
    document.querySelector('[data-xp-binding]')?.focus();
  }
}, true);
document.addEventListener('keydown', event => {
  const tab = event.target.closest('.xp-tabs [role="tab"]');
  if (!tab || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  const tabs = [...tab.parentElement.querySelectorAll('[role="tab"]')];
  const index = tabs.indexOf(tab);
  const next = event.key === 'Home'
    ? 0
    : event.key === 'End'
    ? tabs.length - 1
    : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
  if (tabs[next].dataset.mcpAction) {
    const action = tabs[next].dataset.mcpAction;
    tabs[next].click();
    document.querySelector(`[data-mcp-action="${action}"]`)?.focus();
    return;
  }
  xp.section = tabs[next].dataset.xpSection;
  renderExchangeProject();
  document.querySelector(`[data-xp-section="${xp.section}"]`)?.focus();
}, true);

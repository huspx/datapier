'use strict';
// 所有状态仅存在本页；演示动作不能产生文件、凭据或数据库请求。
const xp = {
  open: '',
  section: 'sources',
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
    filter: '',
    prefix: 'cache:',
    targetPrefix: 'copy:',
    conflict: 'stop',
    mapping: 'order_id → id\namount → total',
    types: 'id: BIGINT\ntotal: DECIMAL(18,2)',
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
  selected: {
    table: true,
    view: true,
    trigger: false,
    procedure: false,
    function: false,
    event: false,
    key: true,
    difference: false,
  },
  confirmed: false,
  prepared: false,
  bindingConfirmed: false,
  projectDirty: false,
  projectActive: false,
  projectBaseline: null,
  savePreview: false,
  conflictDecisions: { added: '', modified: '', deleted: '' },
  pendingLeave: null,
  allowLeave: false,
  previousSelect: '',
  message: '',
};
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
function xpExchangeBody() {
  const mysql = xp.provider === 'mysql',
    file = ['import', 'export', 'sql_dump', 'sql_restore'].includes(xp.mode),
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
    }>MySQL</option><option value="redis"${!mysql ? ' selected' : ''}>Redis</option></select></label>${
      xpField('source', file ? '来源文件或连接' : '来源连接', file ? 'Source file or connection' : 'Source connection')
    }${xpField('target', '目标文件或连接', 'Target file or connection')}${
      ['import', 'export'].includes(xp.mode)
        ? xpSelect('format', '格式', 'Format', [['CSV', 'CSV', 'CSV'], ['JSON', '类型化 JSON', 'Typed JSON']])
        : ''
    }</div><p class="muted">${
      xpLabel(
        '路径与连接均为本页示例，不读取文件或连接数据库。',
        'Paths and connections are examples; no files or databases are accessed.',
      )
    }</p>`;
  }
  if (xp.section === 'objects') {
    return `<h3>${xpLabel('对象与映射', 'Objects and mappings')}</h3><div class="xp-grid">${
      mysql
        ? `${xpField('mapping', '字段／对象映射，每行一项', 'Column / object mapping, one per line', true)}${
          xpField('types', '目标类型', 'Target types', true)
        }${xpField('filter', '筛选条件（留空为全部）', 'Filter (empty means all)')}`
        : `${xpField('prefix', '来源 key 前缀', 'Source key prefix')}${
          xpField('targetPrefix', '目标 key 前缀', 'Target key prefix')
        }<p>${
          xpLabel(
            '支持 String、Hash、List、Set、ZSet；保留绝对到期时间。',
            'String, Hash, List, Set and ZSet; absolute expiry is preserved.',
          )
        }</p>`
    }</div><h4>${xpLabel('选择对象', 'Select objects')}</h4>${
      mysql
        ? xpCheck('table', '表及数据 · orders', 'Table and data · orders')
          + xpCheck('view', '视图 · orders_summary', 'View · orders_summary')
          + xpCheck('trigger', '触发器 · orders_update', 'Trigger · orders_update') + (xp.mode.startsWith('sql_')
            ? xpCheck('procedure', '存储过程 · close_order', 'Procedure · close_order')
              + xpCheck('function', '函数 · order_total', 'Function · order_total')
              + xpCheck('event', '事件 · cleanup', 'Event · cleanup')
            : '')
        : xpCheck('key', '示例 key · cache:example', 'Example key · cache:example')
    }${
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
          xpField('csvColumns', '无表头时的列顺序', 'Column order without a header')
        }${xpField('binaryColumns', '二进制列名', 'Binary column names')}${
          xpField('defaults', '输入列默认值语义', 'Input column default semantics', true)
        }<label class="xp-choice"><input type="checkbox" data-xp-field="header"${xp.fields.header ? ' checked' : ''}> ${
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
    return `<h3>${xpLabel('准备预览 · 示例', 'Prepared preview · example')}</h3><p>${
      xpLabel(
        '样本仅用于展示，后续记录仍可能转换失败。',
        'Samples are illustrative; later records may fail conversion.',
      )
    }</p><div class="xp-table"><table><thead><tr><th>order_id · BIGINT</th><th>amount · DECIMAL</th><th>${
      xpLabel('目标', 'Target')
    }</th></tr></thead><tbody><tr><td>10421</td><td>268.50</td><td>orders</td></tr><tr><td>10422</td><td>128.00</td><td>orders</td></tr></tbody></table></div>${
      xpCheck('difference', '包含结构差异：添加 total 列', 'Include schema difference: add total column')
    }<details><summary>${xpLabel('完整诊断', 'Full diagnostics')}</summary><p>${
      xpLabel(
        '示例：检查目标列与对象依赖；未发送请求。',
        'Example: inspect target columns and object dependencies; no request sent.',
      )
    }</p></details><label class="xp-choice"><input type="checkbox" data-xp-confirm${xp.confirmed ? ' checked' : ''}> ${
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
  }</p></details><button type="button" data-xp-action="resume">${
    xpLabel('预览可续作项（未发送）', 'Preview resumable items (not sent)')
  }</button>`;
}
function xpProjectBody() {
  if (xp.scenario === 'loading') {
    return `<p role="status">${
      xpLabel('正在加载项目示例… 未发送请求。', 'Loading project example… No request was sent.')
    }</p>`;
  }
  if (xp.scenario === 'empty' && xp.section !== 'overview') {
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
    }</p><div class="xp-grid">${xpField('projectName', '项目名称', 'Project name')}${
      xpField('projectPath', '项目目录（示例）', 'Project directory (example)')
    }${xpField('layout', '布局', 'Layout')}</div><div class="xp-actions"><button data-xp-action="create">${
      xpLabel('预览创建', 'Preview create')
    }</button><button data-xp-action="open">${
      xpLabel('预览打开', 'Preview open')
    }</button><button data-xp-action="archive-import">${
      xpLabel('预览导入归档', 'Preview archive import')
    }</button><button data-xp-action="archive-export">${xpLabel('预览导出归档', 'Preview archive export')}</button>${
      xp.projectActive
        ? `<button data-xp-section="bindings">${
          xpLabel('本机绑定', 'Local bindings')
        }</button><button data-xp-section="queries">${xpLabel('查询', 'Queries')}</button>`
        : ''
    }</div><p>${
      xpLabel(
        '归档包含共享项目文件，不包含凭据或本机绑定。导入新目录后必须重新确认绑定；原目录重开沿用已确认绑定。',
        'Archives contain shared project files, never credentials or local bindings. Confirm bindings again after import into a new directory; reopening the original directory retains confirmed bindings.',
      )
    }</p>`;
  }
  if (xp.section === 'bindings') {
    return `<h3>${xpLabel('逻辑连接与本机绑定', 'Logical connections and local bindings')}</h3><div class="xp-grid">${
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
    }</p>`;
  }
  if (xp.section === 'queries') {
    return `<h3>${xpLabel('查询', 'Queries')}</h3><div class="xp-grid">${
      xpField('query', 'MySQL 查询正文', 'MySQL query text', true)
    }${xpField('redisQuery', 'Redis 命令正文', 'Redis command text', true)}</div><p>${
      xpLabel(
        '多行文本为项目草稿，切换分区保留；不会执行。',
        'Multiline text is a project draft, retained across sections; it is never executed.',
      )
    }</p>${
      xp.fields.externalQuery
        ? `<label class="xp-field"><span>queries/new.sql</span><textarea readonly rows="3">${
          h(xp.fields.externalQuery)
        }</textarea></label>`
        : ''
    }`;
  }
  if (xp.section === 'transfers') {
    return `<h3>${xpLabel('迁移配置', 'Transfer configuration')}</h3><div class="xp-grid">${
      xpField('transferName', '配置名称', 'Configuration name')
    }${xpField('transferConfig', '配置正文', 'Configuration text', true)}</div><p>transfers/old.json · ${
      xp.fields.oldTransfer ? xpLabel('保留在草稿', 'Retained in draft') : xpLabel('已从草稿删除', 'Removed from draft')
    }</p>`;
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
        : ''
    }<button data-xp-section="recovery">${xpLabel('查看外部冲突示例', 'Review example external conflicts')}</button>`;
  }
  return `<h3>${xpLabel('冲突恢复与遗留资源', 'Conflict recovery and leftover resources')}</h3><p>${
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
  }</button><details><summary>${
    xpLabel('遗留 partial 文件身份与摘要', 'Partial file identity and summary')
  }</summary><p>orders.csv.partial · ${
    xpLabel('示例身份待核对；不是完整导出。', 'Example identity requires verification; not a complete export.')
  }</p></details><label class="xp-choice"><input type="checkbox" data-xp-confirm${xp.confirmed ? ' checked' : ''}> ${
    xpLabel('已核对身份，预览领取', 'Identity reviewed; preview claim')
  }</label><button data-xp-action="claim"${xp.confirmed ? '' : ' disabled'}>${
    xpLabel('预览领取（未发送）', 'Preview claim (not sent)')
  }</button>`;
}
function renderExchangeProject() {
  if (!xp.open) return;
  const area = document.querySelector('#app .workarea');
  if (!area) return;
  const exchange = xp.open === 'exchange';
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
  }</p></div><button data-xp-action="close">${
    xpLabel('返回工作区', 'Return to workspace')
  }</button></header><div class="xp-summary"><strong>${xpLabel('目标摘要', 'Target summary')}</strong><span>${
    exchange
      ? `${h(xp.provider.toUpperCase())} · ${h(xp.fields.source)} → ${h(xp.fields.target)}`
      : `${h(xp.fields.projectName)} · ${h(xp.fields.projectPath)}`
  }</span><span>${readOnly ? xpLabel('只读示例', 'Read-only example') : xpLabel('未发送', 'Not sent')}</span></div>${
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
      ? `<button data-xp-action="prepare"${readOnly ? ' disabled' : ''}>${
        xpLabel('准备预览', 'Prepare preview')
      }</button><button class="primary" data-xp-action="execute"${
        readOnly || !xp.prepared || !xp.confirmed ? ' disabled' : ''
      }>${xpLabel('预览执行（未发送）', 'Preview execution (not sent)')}</button><button data-xp-action="cancel"${
        xp.scenario === 'running' ? '' : ' disabled'
      }>${xpLabel('预览取消', 'Preview cancellation')}</button>`
      : `<button data-xp-action="save-preview">${
        xpLabel('查看保存差异', 'Review save changes')
      }</button><button class="primary" data-xp-action="save"${readOnly || !xp.savePreview ? ' disabled' : ''}>${
        xpLabel('确认示例保存（未发送）', 'Confirm example save (not sent)')
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
  xp.message = '';
  state.overlay = null;
  state.menu = null;
  render();
}
document.addEventListener('click', event => {
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
  const field = event.target.dataset.xpField;
  if (!field) return;
  xp.fields[field] = event.target.type === 'checkbox' ? event.target.checked : event.target.value;
  if (field === 'binding' || field === 'alias') {
    xp.bindingConfirmed = false;
    const confirmation = document.querySelector('[data-xp-binding]');
    if (confirmation) confirmation.checked = false;
  }
  const summary = document.querySelector('.xp-summary span:nth-child(2)');
  if (summary) {
    summary.textContent = xp.open === 'exchange'
      ? `${xp.provider.toUpperCase()} · ${xp.fields.source} → ${xp.fields.target}`
      : `${xp.fields.projectName} · ${xp.fields.projectPath}`;
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
          ? 'recovery'
          : part,
      );
      return;
    }
    xp.open = '';
  }
  if (target.id === 'preview-source') xp.open = '';
  if (target.hasAttribute('data-xp-mode')) {
    xp.mode = target.value;
    xp.prepared = false;
    renderExchangeProject();
  }
  if (target.hasAttribute('data-xp-provider')) {
    xp.provider = target.value;
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
  xp.section = tabs[next].dataset.xpSection;
  renderExchangeProject();
  document.querySelector(`[data-xp-section="${xp.section}"]`)?.focus();
}, true);

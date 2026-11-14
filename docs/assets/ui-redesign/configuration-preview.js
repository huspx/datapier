'use strict';
// 配置与权限只操作页面内示例，不调用应用或数据库 API。
const configText = (zh, en) => language === 'zh-CN' ? zh : en;
const configMessage = message => Array.isArray(message) ? configText(...message) : message;
const configCopy = value => JSON.parse(JSON.stringify(value));
const configDefaults = {
  mysqlRows: '100',
  sqlRows: '100',
  redisRows: '100',
  historyRows: '20',
  archeryRows: '100',
  redisKeys: '1000',
  requestRows: '100',
  maxRows: '1000',
  responseBytes: '1048576',
  previewBytes: '4096',
  connectTimeout: '10',
  queryTimeout: '30',
  countTimeout: '10',
  resultIdle: '300',
  resultMemory: '67108864',
  mcpTasks: '4',
  approvalTtl: '600',
  retentionDays: '',
  maxEntries: '',
  planItems: '10000',
  planBytes: '33554432',
  valueBytes: '16777216',
  textBytes: '1048576',
  previewRows: '100',
  editorMode: 'disabled',
  editorProgram: '',
  editorArgs: '',
  cleanup: false,
  approvalNotice: true,
  authNotice: true,
  defaultPolicy: 'dangerous',
  fineGrained: false,
};
const configPreview = {
  saved: configCopy(configDefaults),
  draft: configCopy(configDefaults),
  tab: 'general',
  message: '',
  profile: '',
  ruleState: 'idle',
  ruleMessage: '',
  scope: '',
  level: 'read_only',
  savedRules: {
    mysql: { commerce: 'read_only' },
    redis: { '*': 'read_only' },
    'archery-mysql': { commerce: 'read_only' },
    'archery-redis': { '*': 'read_only' },
  },
  ruleDrafts: {},
  rules: {},
  policy: 'dangerous',
  savedPolicies: {},
  confirm: '',
  confirmFocus: null,
  errors: {},
  fileState: 'loaded',
  fileSelected: '',
  fileClaims: {},
  fileConfirm: null,
  busy: false,
  settingsBusy: false,
};
function redisEffectiveCount(form = null) {
  const override = form ? form.redisDatabasesOverride : state.redisDatabasesOverride;
  const number = Number(override);
  if (override !== '' && Number.isInteger(number) && number > 0 && number <= 4294967295) return number;
  return state.redisDetection === 'loaded' ? state.redisDetectedCount : null;
}
function redisDbOptions(current, count) {
  current = Number(current);
  const displayed = Math.min(count || 0, 1024);
  const values = Array.from({ length: displayed }, (_, n) => n);
  if (!values.includes(current)) values.push(current);
  return values.map(n =>
    `<option value="${n}" ${n === current ? 'selected' : ''}>DB ${n}${
      count !== null && n >= count ? ` (${h(configText('超出数量', 'outside range'))})` : ''
    }</option>`
  ).join('');
}
function redisConnectionPreview(form) {
  const count = redisEffectiveCount(form);
  const status = form.redisDatabasesOverride && count !== null
    ? configText(`手动覆盖：${count} 个 DB`, `Manual override: ${count} databases`)
    : state.redisDetection === 'loading'
    ? configText('示例探测中…', 'Simulating detection…')
    : state.redisDetection === 'error'
    ? configText('示例探测失败；数量未知', 'Simulated detection failed; count unknown')
    : count === null
    ? configText('数量未知', 'Count unknown')
    : configText(`示例探测：${count} 个 DB`, `Example detection: ${count} databases`);
  return `<label class="wide">${h(t('initialDb'))}<select data-connection-field="redisDb">${
    redisDbOptions(form.redisDb, count)
  }</select></label>
    <label class="wide">${
    h(configText(
      'Redis DB 数量覆盖（1–4294967295；留空自动探测）',
      'Redis database count override (1–4294967295; blank for automatic detection)',
    ))
  }<input type="number" min="1" max="4294967295" step="1" data-config-redis-override value="${
    h(form.redisDatabasesOverride)
  }"></label>
    <div class="wide config-inline"><span class="muted small" role="status">${h(status)}${
    count > 1024 ? ` · ${h(configText('仅列出前 1024 项及当前 DB', 'Showing first 1024 and current DB'))}` : ''
  } · ${h(configText('仅为示例，不连接数据库', 'Example only; no database connection'))}</span>
    <button type="button" class="button" data-config-action="redis-detect" ${
    state.redisDetection === 'loading' ? 'disabled' : ''
  }>${h(configText('模拟探测', 'Simulate detection'))}</button>
    <button type="button" class="button" data-config-action="redis-fail" ${
    state.redisDetection === 'loading' ? 'disabled' : ''
  }>${h(configText('模拟失败', 'Simulate failure'))}</button></div>`;
}
function configPolicyLabel(policy) {
  return ({
    off: configText('关闭审批', 'Approval off'),
    dangerous: configText('危险操作审批', 'Dangerous operations'),
    all_writes: configText('所有写入审批', 'All writes'),
  })[policy];
}
function configRow(label, control, required = false) {
  return `<div class="config-row"><span>${h(label)}${
    required ? ' <span class="required-mark" aria-hidden="true">*</span>' : ''
  }</span>${control}</div>`;
}
function configPolicySelect(selected, owner, values = ['off', 'dangerous', 'all_writes']) {
  const label = owner === 'default'
    ? configText('新连接默认审批策略', 'Default approval policy for new connections')
    : configText('连接审批策略', 'Connection approval policy');
  return `<select data-config-policy-owner="${owner}" aria-label="${h(label)}" ${
    owner === 'rule' && configPreview.busy ? 'disabled' : ''
  }>${
    values.map(value =>
      `<option value="${value}" ${selected === value ? 'selected' : ''}>${h(configPolicyLabel(value))}</option>`
    ).join('')
  }</select>`;
}
function configNumber(key, zh, en, optional = false) {
  const max = ['mysqlRows', 'sqlRows', 'redisRows', 'historyRows', 'archeryRows', 'maxRows'].includes(key)
    ? 1000
    : key === 'responseBytes'
    ? 1048576
    : null;
  const min = key === 'responseBytes' ? 1024 : 1;
  const label = configText(zh, en), error = configPreview.errors[key];
  const hint = `${min}${max ? `–${max}` : '+'}${
    optional ? configText('；可留空', '; optional') : configText('；必填', '; required')
  }`;
  return `<div class="config-row"><label class="config-number-label" for="config-input-${key}">${h(label)}${
    optional ? '' : ' <span class="required-mark" aria-hidden="true">*</span>'
  }<small id="config-hint-${key}" class="${error ? 'config-error' : 'muted'}">${h(error || hint)}</small></label>
    <input id="config-input-${key}" type="number" min="${min}" ${max ? `max="${max}"` : ''}
    step="1" data-config-field="${key}" aria-label="${h(label)}" aria-describedby="config-hint-${key}"
    ${error ? 'aria-invalid="true"' : ''} value="${h(configPreview.draft[key])}" ${optional ? '' : 'required'}></div>`;
}
function configSwitch(key, zh, en) {
  const label = configText(zh, en);
  return configRow(
    label,
    `<button type="button" class="config-switch" role="switch" aria-label="${h(label)}" aria-checked="${
      configPreview.draft[key]
    }" data-config-action="switch" data-config-key="${key}"><span></span></button>`,
  );
}
function configDirty() {
  return JSON.stringify(configPreview.draft) !== JSON.stringify(configPreview.saved);
}
function configRuleDirty(profile = configPreview.profile) {
  const c = configPreview;
  if (!profile) return false;
  const draft = profile === c.profile ? { rules: c.rules, policy: c.policy } : c.ruleDrafts[profile];
  return !!draft
    && (JSON.stringify(draft.rules) !== JSON.stringify(c.savedRules[profile])
      || draft.policy !== c.savedPolicies[profile]);
}
function configAnyRuleDirty() {
  return Object.keys(configPreview.savedRules).some(configRuleDirty);
}
function configRequestClose() {
  const c = configPreview;
  if (c.settingsBusy || c.busy) {
    c.message = ['正在模拟保存或加载，请稍候。', 'A simulated save or load is in progress; please wait.'];
    render();
    return true;
  }
  if (!configDirty() && !configAnyRuleDirty()) return false;
  c.confirmFocus = document.activeElement?.getAttribute('data-config-action') || 'reload-settings';
  c.confirm = 'close';
  render();
  document.querySelector('[data-config-action="cancel-confirm"]')?.focus();
  return true;
}
function configCancelConfirm() {
  const focus = configPreview.confirmFocus;
  configPreview.confirm = '';
  configPreview.fileConfirm = null;
  render();
  document.querySelector(`[data-config-action="${focus || 'reload-settings'}"]`)?.focus();
}
function configStatus() {
  return configDirty()
    ? configText('全局设置：未保存', 'Global settings: unsaved')
    : configText('全局设置：无未保存修改', 'Global settings: no unsaved changes');
}
function configRefreshStatus() {
  const status = document.querySelector('[data-config-status]');
  if (status) status.textContent = configStatus();
}
function configValidate() {
  const c = configPreview, d = c.draft, errors = {};
  const numbers = [
    'mysqlRows',
    'sqlRows',
    'redisRows',
    'historyRows',
    'archeryRows',
    'redisKeys',
    'requestRows',
    'maxRows',
    'responseBytes',
    'previewBytes',
    'connectTimeout',
    'queryTimeout',
    'countTimeout',
    'resultIdle',
    'resultMemory',
    'mcpTasks',
    'approvalTtl',
    'retentionDays',
    'maxEntries',
    'planItems',
    'planBytes',
    'valueBytes',
    'textBytes',
    'previewRows',
  ];
  for (const key of numbers) {
    const v = d[key], optional = ['retentionDays', 'maxEntries'].includes(key);
    const max = ['mysqlRows', 'sqlRows', 'redisRows', 'historyRows', 'archeryRows', 'maxRows'].includes(key)
      ? 1000
      : key === 'responseBytes'
      ? 1048576
      : Infinity;
    const min = key === 'responseBytes' ? 1024 : 1;
    if (!(optional && v === '') && (!/^\d+$/.test(v) || Number(v) < min || Number(v) > max)) {
      errors[key] = configText(
        `请输入 ${min}${max < Infinity ? `–${max}` : ' 以上'}的整数。`,
        `Enter an integer ${min}${max < Infinity ? `–${max}` : ' or greater'}.`,
      );
    }
  }
  if (!errors.textBytes && !errors.valueBytes && BigInt(d.textBytes) > BigInt(d.valueBytes)) {
    errors.textBytes = configText('不能超过单值字节上限。', 'Cannot exceed the value byte limit.');
  }
  if (d.editorMode !== 'disabled') {
    if (!d.editorProgram.trim() || d.editorProgram.includes('\0')) {
      errors.editorProgram = configText('请输入有效的编辑器程序。', 'Enter a valid editor program.');
    }
    if ((d.editorArgs.match(/\{file\}/g) || []).length !== 1 || d.editorArgs.includes('\0')) {
      errors.editorArgs = configText('参数必须恰好包含一个 {file}。', 'Arguments must contain exactly one {file}.');
    }
  }
  c.errors = errors;
  return Object.keys(errors)[0];
}
function configEditorField(key, zh, en, multiline = false) {
  const label = configText(zh, en), error = configPreview.errors[key], value = h(configPreview.draft[key]);
  const attrs = `data-config-field="${key}" aria-label="${h(label)}" aria-describedby="config-hint-${key}" ${
    error ? 'aria-invalid="true"' : ''
  }`;
  return configRow(
    label,
    `<div class="config-field">${
      multiline ? `<textarea ${attrs}>${value}</textarea>` : `<input ${attrs} value="${value}">`
    }<small id="config-hint-${key}" class="${error ? 'config-error' : 'muted'}">${
      h(
        error || (multiline
          ? configText(
            '每行一个参数；恰好一个 {file}，不经 shell。',
            'One argument per line; exactly one {file}; no shell.',
          )
          : configText('启用编辑器时必填。', 'Required when enabled.')),
      )
    }</small></div>`,
    true,
  );
}
function configLevelLabel(level) {
  return ({
    disabled: configText('禁用', 'Disabled'),
    read_only: configText('只读', 'Read only'),
    read_write: configText('读写', 'Read and write'),
  })[level];
}
const configFiles = [
  {
    id: 'draft',
    name: '订单草稿 · JSON',
    source: 'Commerce / orders',
    path: '/example/orders.json',
    size: '4096 B',
    referenced: false,
    active: false,
    version: '1',
  },
  {
    id: 'referenced',
    name: '查询草稿 · Text',
    source: 'Commerce / query',
    path: '/example/query.txt',
    size: '未记录',
    referenced: true,
    active: false,
    version: '2',
  },
];
function configFilesPreview() {
  const c = configPreview,
    file = configFiles.find(item => item.id === c.fileSelected),
    claimed = file && c.fileClaims[file.id] === file.version;
  const missing = configText('未记录', 'Not recorded');
  const lists = c.fileState === 'loading'
    ? configText('正在模拟加载…', 'Simulating loading…')
    : c.fileState === 'error'
    ? configText('模拟加载失败；可重试。', 'Simulated load failure; retry available.')
    : c.fileState === 'empty'
    ? configText('示例列表为空。', 'Example list is empty.')
    : '';
  return `<section class="config-section config-section-wide"><h3>${
    h(configText('遗留编辑文件', 'Leftover editing files'))
  }</h3><p class="muted small">${
    h(configText(
      '仅为示例，不访问文件；领取仅用于管理文件，不恢复数据库编辑权限。',
      'Example only; no files are accessed. Claiming manages files and does not restore database editing access.',
    ))
  }</p><div class="config-inline">${
    ['loaded', 'empty', 'loading', 'error'].map((value, i) =>
      `<button type="button" class="button" data-config-action="file-state" data-value="${value}">${
        h(
          [
            configText('示例列表', 'Example list'),
            configText('空列表', 'Empty'),
            configText('加载中', 'Loading'),
            configText('加载失败', 'Failure'),
          ][i],
        )
      }</button>`
    ).join('')
  }</div><p role="status">${h(lists)}</p>${
    c.fileState === 'loaded'
      ? configFiles.map(item =>
        `<button type="button" class="button config-file-row" data-config-action="file-select" data-value="${item.id}" aria-pressed="${
          item.id === c.fileSelected
        }">${h(item.name)} · ${h(item.source)} · ${h(item.size === '未记录' ? missing : item.size)} · ${
          h(configText('时间未记录', 'Time not recorded'))
        }</button>`
      ).join('')
      : ''
  }${
    file && c.fileState === 'loaded'
      ? `<div class="config-file-detail"><strong>${h(file.name)}</strong><dl><dt>${
        h(configText('来源', 'Source'))
      }</dt><dd>${h(file.source)}</dd><dt>${h(configText('路径（示例）', 'Path (example)'))}</dt><dd>${
        h(file.path)
      }</dd><dt>${h(configText('大小', 'Size'))}</dt><dd>${h(file.size === '未记录' ? missing : file.size)}</dd><dt>${
        h(configText('状态', 'State'))
      }</dt><dd>${h(claimed ? configText('示例已领取', 'Example claimed') : configText('遗留', 'Leftover'))}</dd><dt>${
        h(configText('引用', 'Reference'))
      }</dt><dd>${
        h(
          file.referenced
            ? configText('草稿引用中', 'Referenced by draft')
            : configText('无草稿引用', 'Not referenced'),
        )
      }</dd><dt>${h(configText('领取版本', 'Claim version'))}</dt><dd>${
        h(claimed ? file.version : missing)
      }</dd></dl><p class="muted small">${
        h(
          file.referenced
            ? configText('草稿引用中，不能领取或清理。', 'Referenced by a draft; cannot claim or clean up.')
            : configText(
              '领取为本页状态演示；导出和清理不会操作文件。',
              'Claim is a page-only example; export and cleanup do not access files.',
            ),
        )
      }</p><div class="config-inline"><button type="button" class="button" data-config-action="file-claim" ${
        file.referenced || file.active || claimed ? 'disabled' : ''
      }>${h(configText('模拟领取', 'Simulate claim'))}</button><button type="button" class="button" disabled>${
        h(configText('导出文件（示例不可用）', 'Export file (unavailable in example)'))
      }</button><button type="button" class="button" data-config-action="file-cleanup" ${
        !claimed || file.referenced ? 'disabled' : ''
      }>${h(configText('查看清理确认', 'Review cleanup'))}</button></div></div>`
      : ''
  }</section>`;
}
function settingsPreview() {
  const c = configPreview, d = c.draft;
  if (c.confirm) {
    const f = c.fileConfirm;
    return `<div class="config-preview"><p>${
      h(
        c.confirm === 'reload'
          ? configText(
            '重新加载将丢弃全局设置草稿，连接权限草稿保留。',
            'Reload discards global settings drafts; connection drafts are retained.',
          )
          : c.confirm === 'close'
          ? configText(
            '关闭将丢弃未保存的全局设置和连接权限草稿。',
            'Closing discards unsaved global settings and connection drafts.',
          )
          : configText(
            '清理确认示例：以下目标已冻结。实际清理不可恢复；此示例不会访问文件。',
            'Cleanup confirmation example: the target is frozen. Actual cleanup is irreversible; this example does not access files.',
          ),
      )
    }</p>${
      f
        ? `<dl><dt>${h(configText('文件', 'File'))}</dt><dd>${h(f.name)}</dd><dt>${
          h(configText('路径', 'Path'))
        }</dt><dd>${h(f.path)}</dd><dt>${h(configText('大小', 'Size'))}</dt><dd>${h(f.size)}</dd><dt>${
          h(configText('引用', 'Reference'))
        }</dt><dd>${h(configText('无草稿引用', 'Not referenced'))}</dd><dt>${
          h(configText('领取版本', 'Claim version'))
        }</dt><dd>${h(f.version)}</dd></dl>`
        : ''
    }</div>`;
  }
  const languageSelect = `<select id="language-preference" aria-label="${h(t('language'))}">${
    ['system', 'zh-CN', 'en'].map(value =>
      `<option value="${value}" ${languagePreference === value ? 'selected' : ''}>${
        h(t(value === 'system' ? 'system' : value === 'zh-CN' ? 'chinese' : 'english'))
      }</option>`
    ).join('')
  }</select>`;
  const general = `<div class="config-language">${configRow(t('language'), languageSelect)}</div>
    <div class="config-grid"><section class="config-section"><h3>${h(configText('分页', 'Paging'))}</h3>${
    configNumber('mysqlRows', 'MySQL 表页行数', 'MySQL table rows per page')
  }${configNumber('sqlRows', 'SQL 页行数', 'SQL rows per page')}${
    configNumber('redisRows', 'Redis 值页行数', 'Redis value rows per page')
  }${configNumber('historyRows', '历史页行数', 'History rows per page')}${
    configNumber('archeryRows', '平台结果页行数', 'Platform result rows per page')
  }${configNumber('redisKeys', 'Redis key 批量', 'Redis key batch')}${
    configNumber('requestRows', '平台请求行数', 'Platform request rows')
  }</section><section class="config-section"><h3>${h(configText('超时设置', 'Timeouts'))}</h3>${
    configNumber('connectTimeout', '连接超时（秒）', 'Connect timeout (seconds)')
  }${configNumber('queryTimeout', '执行超时（秒）', 'Execution timeout (seconds)')}${
    configNumber('countTimeout', '计数超时（秒）', 'Count timeout (seconds)')
  }${configNumber('resultIdle', '结果空闲回收（秒）', 'Result idle time (seconds)')}
    </section><section class="config-section"><h3>${h(configText('资源限制', 'Resource limits'))}</h3>${
    configNumber('responseBytes', '响应字节上限', 'Response byte limit')
  }${configNumber('previewBytes', '值预览字节数', 'Value preview bytes')}${
    configNumber('resultMemory', '客户端结果内存字节数', 'Result memory bytes per client')
  }</section><section class="config-section"><h3>${h(configText('历史', 'History'))}</h3>${
    configSwitch('cleanup', '自动清理历史', 'Automatically clean up history')
  }${configNumber('retentionDays', '保留天数（可留空）', 'Retention days (optional)', true)}${
    configNumber('maxEntries', '最大条数（可留空）', 'Maximum entries (optional)', true)
  }</section></div>`;
  const editing = `<div class="config-grid"><section class="config-section"><h3>${
    h(configText('编辑限制', 'Editing limits'))
  }</h3>${configNumber('planItems', '计划条目上限（条）', 'Maximum plan items')}${
    configNumber('planBytes', '计划字节上限（字节）', 'Maximum plan bytes')
  }${configNumber('valueBytes', '单值字节上限（字节）', 'Maximum value bytes')}${
    configNumber('textBytes', '文本编辑字节上限（字节）', 'Maximum text editor bytes')
  }${configNumber('previewRows', '预览每页条目数', 'Preview items per page')}<p class="muted small">${
    h(configText('文本编辑上限不能超过单值上限。', 'Text editor limit cannot exceed the value limit.'))
  }</p></section>
    <section class="config-section"><h3>${h(configText('外部编辑器', 'External editor'))}</h3>${
    configRow(
      configText('编辑器模式', 'Editor mode'),
      `<select data-config-field="editorMode" aria-label="${h(configText('编辑器模式', 'Editor mode'))}">${
        [['disabled', configText('不配置', 'Disabled')], ['gui', configText('图形界面', 'Graphical')], [
          'terminal',
          configText('终端', 'Terminal'),
        ]].map(([v, l]) => `<option value="${v}" ${d.editorMode === v ? 'selected' : ''}>${h(l)}</option>`).join('')
      }</select>`,
    )
  }${
    d.editorMode === 'disabled'
      ? ''
      : configEditorField('editorProgram', '编辑器程序', 'Editor program')
        + configEditorField('editorArgs', '编辑器参数', 'Editor arguments', true)
  }</section>${configFilesPreview()}</div>`;
  const archery =
    `<div class="config-grid"><section class="config-section config-section-wide config-servers">${serverList()}</section></div>`;
  const mcp = `<div class="config-grid"><section class="config-section"><h3>${
    h(configText('资源限制', 'Resource limits'))
  }</h3>${configNumber('maxRows', 'MCP 最大行数', 'Maximum MCP rows')}${
    configNumber('mcpTasks', 'MCP 并行任务数', 'Running tasks per MCP client')
  }</section>
    <section class="config-section"><h3>MCP</h3>${
    configSwitch('fineGrained', '启用 MCP 细粒度权限', 'Enable fine-grained MCP permissions')
  }</section><section class="config-section"><h3>${h(configText('通知', 'Notifications'))}</h3>${
    configSwitch('approvalNotice', '审批通知', 'Approval notifications')
  }${configSwitch('authNotice', '认证通知', 'Authentication notifications')}</section>
    <section class="config-section"><h3>${h(configText('审批', 'Approval'))}</h3>${
    configNumber('approvalTtl', '审批有效期（秒）', 'Approval TTL (seconds)')
  }${
    configRow(
      configText('新连接默认审批策略', 'Default approval policy for new connections'),
      configPolicySelect(d.defaultPolicy, 'default', ['dangerous', 'all_writes']),
    )
  }</section><section class="config-section config-section-wide"><h3>${
    h(configText('MCP 连接权限', 'MCP connection permissions'))
  }</h3>${
    configRow(
      configText('连接', 'Connection'),
      `<select data-config-profile aria-label="${h(configText('连接', 'Connection'))}" ${
        c.busy ? 'disabled' : ''
      }><option value="">${h(configText('选择连接', 'Select a connection'))}</option>${
        [
          ['mysql', 'Commerce · Direct MySQL'],
          ['redis', 'Cache · Direct Redis'],
          ['archery-mysql', 'Archery · MySQL'],
          ['archery-redis', 'Archery · Redis'],
        ].map(([value, label]) =>
          `<option value="${value}" ${c.profile === value ? 'selected' : ''}>${h(label)}</option>`
        ).join('')
      }</select>`,
    )
  }<div class="config-inline"><button type="button" class="button" data-config-action="load-error" ${
    c.busy ? 'disabled' : ''
  }>${h(configText('模拟加载失败', 'Simulate load failure'))}</button></div>
    <p class="${c.ruleState === 'error' ? 'config-error' : 'muted'} small" role="status">${
    h(
      configMessage(c.ruleMessage)
        || configText('选择连接以加载示例权限。', 'Select a connection to load example permissions.'),
    )
  }</p>${c.profile ? configRulesPreview() : ''}</section></div>`;
  return `<div class="config-preview"><p class="config-caption muted small">${
    h(configText(
      '本页仅为示例，不连接数据库或保存真实配置。',
      'Page example only; no database connection or real settings are saved.',
    ))
  }</p>
    <div class="config-tabs" role="tablist" aria-label="${h(t('settings'))}">${
    [['general', configText('常规', 'General')], ['editing', configText('编辑与文件', 'Editing and files')], [
      'archery',
      'Archery',
    ], ['mcp', 'MCP']].map(([tab, label]) =>
      `<button type="button" role="tab" aria-selected="${c.tab === tab}" tabindex="${
        c.tab === tab ? '0' : '-1'
      }" aria-controls="config-panel" data-config-action="tab" data-config-tab="${tab}">${h(label)}</button>`
    ).join('')
  }</div><p role="status" class="config-status" data-config-status>${h(configStatus())}</p>
    <div id="config-panel" role="tabpanel" aria-label="${
    h(
      ({
        general: configText('常规', 'General'),
        editing: configText('编辑与文件', 'Editing and files'),
        archery: 'Archery',
        mcp: 'MCP',
      })[c.tab],
    )
  }">${({ general, editing, archery, mcp })[c.tab]}</div>
    <p role="status" class="muted small">${h(configMessage(c.message))}</p>
    <details class="config-demo-controls"><summary>${
    h(configText('示例说明与状态', 'Example notes and states'))
  }</summary>
    <p class="muted small">${
    h(configText(
      '全局保存仅保存全局设置；连接策略和权限单独保存。语言偏好保存在浏览器中。',
      'Global save covers global settings; save connection policy and permissions separately. Language preference is stored in this browser.',
    ))
  }</p>${
    c.tab === 'archery'
      ? ''
      : `<button type="button" class="button" data-config-action="save-settings-error" ${
        c.settingsBusy ? 'disabled' : ''
      }>${h(configText('模拟全局保存失败', 'Simulate global save failure'))}</button>`
  }
    </details></div>`;
}
function settingsPreviewFooter() {
  if (configPreview.confirm) {
    return `<button type="button" class="button" data-config-action="cancel-confirm">${
      h(configText('继续编辑', 'Continue editing'))
    }</button><span class="spacer"></span>${
      configPreview.confirm === 'file'
        ? `<button type="button" class="button" disabled>${
          h(configText('确认清理（示例不可用）', 'Confirm cleanup (unavailable in example)'))
        }</button>`
        : `<button type="button" class="button danger" data-config-action="confirm-discard">${
          h(configText('丢弃', 'Discard'))
        }</button>`
    }`;
  }
  if (configPreview.tab === 'archery') {
    return `<span class="spacer"></span><button type="button" class="button" data-action="close-modal">${
      h(configText('关闭', 'Close'))
    }</button>`;
  }
  return `<button type="button" class="button" data-config-action="reload-settings" ${
    configPreview.settingsBusy ? 'disabled' : ''
  }>${
    h(configText('重新加载设置', 'Reload settings'))
  }</button><span class="spacer"></span><button type="button" class="button primary" data-config-action="save-settings" ${
    configPreview.settingsBusy ? 'disabled' : ''
  }>${h(configText('保存全局设置', 'Save global settings'))}</button>`;
}
function configRulesPreview() {
  const c = configPreview, redis = c.profile.endsWith('redis');
  const rows = Object.entries(c.rules).map(([scope, level]) =>
    configRow(
      redis && scope === '*' ? configText('Redis 整个实例', 'Entire Redis instance') : scope,
      `<span>${h(configLevelLabel(level))}</span>`,
    )
  ).join('');
  return `<p class="muted small" role="status">${
    h(
      configRuleDirty()
        ? configText('当前连接策略和权限：未保存', 'Current connection policy and permissions: unsaved')
        : configText(
          '当前连接策略和权限：无未保存修改',
          'Current connection policy and permissions: no unsaved changes',
        ),
    )
  }</p>${configRow(configText('连接审批策略', 'Connection approval policy'), configPolicySelect(c.policy, 'rule'))}${
    c.draft.fineGrained
      ? `<div class="config-rule-list">${rows}</div>${
        redis ? '' : configRow(
          configText('MySQL 数据库名称', 'MySQL database name'),
          `<input data-config-field="scope" aria-label="${
            h(configText('MySQL 数据库名称', 'MySQL database name'))
          }" value="${h(c.scope)}" ${c.busy ? 'disabled' : ''}>`,
          true,
        )
      }${
        configRow(
          configText('访问级别', 'Access level'),
          `<select data-config-level aria-label="${h(configText('访问级别', 'Access level'))}" ${
            c.busy ? 'disabled' : ''
          }>${
            ['disabled', 'read_only', 'read_write'].map(level =>
              `<option value="${level}" ${c.level === level ? 'selected' : ''}>${h(configLevelLabel(level))}</option>`
            ).join('')
          }</select>`,
        )
      }<div class="config-inline"><button type="button" class="button" data-config-action="set-rule" ${
        c.busy ? 'disabled' : ''
      }>${h(configText('加入权限草稿', 'Add to permission draft'))}</button></div>`
      : ''
  }<div class="config-inline"><button type="button" class="button" data-config-action="save-error" ${
    c.busy ? 'disabled' : ''
  }>${
    h(configText('模拟保存失败', 'Simulate save failure'))
  }</button><button type="button" class="button primary" data-config-action="save-rules" ${c.busy ? 'disabled' : ''}>${
    h(configText('保存当前连接策略和权限示例', 'Save current connection policy and permissions example'))
  }</button></div>`;
}
document.addEventListener('keydown', event => {
  const tab = event.target.closest('[data-config-action="tab"]');
  if (!tab || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  event.preventDefault();
  const tabs = ['general', 'editing', 'archery', 'mcp'];
  const current = tabs.indexOf(tab.dataset.configTab);
  const next = event.key === 'Home'
    ? 0
    : event.key === 'End'
    ? tabs.length - 1
    : (current + (event.key === 'ArrowLeft' ? -1 : 1) + tabs.length) % tabs.length;
  configPreview.tab = tabs[next];
  render();
  document.querySelector(`[data-config-tab="${configPreview.tab}"]`)?.focus();
});
document.addEventListener('input', event => {
  const field = event.target.dataset.configField;
  if (field && field !== 'scope' && event.target.type !== 'checkbox') {
    configPreview.draft[field] = event.target.value;
    delete configPreview.errors[field];
    configRefreshStatus();
  }
  if (field === 'scope') {
    configPreview.scope = event.target.value;
    const level = configPreview.rules[event.target.value];
    if (level) {
      configPreview.level = level;
      const select = document.querySelector('[data-config-level]');
      if (select) select.value = level;
    }
  }
});
document.addEventListener('change', event => {
  const target = event.target, field = target.dataset.configField;
  if (target.hasAttribute('data-config-redis-override')) {
    state.connectionForm.redisDatabasesOverride = target.value;
    render();
    return;
  }
  const c = configPreview;
  if (target.hasAttribute('data-config-profile')) {
    if (c.profile) {
      c.ruleDrafts[c.profile] = { rules: configCopy(c.rules), scope: c.scope, level: c.level, policy: c.policy };
    }
    c.profile = target.value;
    if (c.profile) {
      const draft = c.ruleDrafts[c.profile];
      c.rules = configCopy(draft ? draft.rules : c.savedRules[c.profile]);
      c.scope = draft ? draft.scope : '';
      c.level = draft ? draft.level : (c.profile.endsWith('redis') ? c.rules['*'] : null) || 'read_only';
      if (!(c.profile in c.savedPolicies)) c.savedPolicies[c.profile] = state.connectionApprovalPolicies[c.profile];
      c.policy = draft ? draft.policy : c.savedPolicies[c.profile];
      c.ruleState = 'loaded';
      c.ruleMessage = ['已加载本页示例权限，未发送请求。', 'Loaded example permissions; no request was sent.'];
    }
    render();
    return;
  }
  if (target.dataset.configPolicyOwner) {
    if (target.dataset.configPolicyOwner === 'default') c.draft.defaultPolicy = target.value;
    else {
      c.policy = target.value;
      c.ruleMessage = ['仅修改本页示例，未发送。', 'Changed in this example only; nothing was sent.'];
    }
    render();
    return;
  }
  if (target.hasAttribute('data-config-level')) {
    c.level = target.value;
    return;
  }
  if (!field) return;
  if (field === 'scope') configPreview.scope = target.value;
  else {
    configPreview.draft[field] = target.type === 'checkbox' ? target.checked : target.value;
    if (field === 'editorMode') render();
  }
});
document.addEventListener('click', event => {
  const button = event.target.closest('[data-config-action]');
  if (!button || button.disabled) return;
  const action = button.dataset.configAction;
  const c = configPreview;
  if (action === 'tab') {
    c.tab = button.dataset.configTab;
    render();
  } else if (action === 'switch') {
    c.draft[button.dataset.configKey] = !c.draft[button.dataset.configKey];
    render();
  } else if (action === 'redis-detect') {
    state.redisDetection = 'loading';
    render();
    setTimeout(() => {
      state.redisDetectedCount = 16;
      state.redisDetection = 'loaded';
      if (state.connectionForm && ['new-connection', 'connection-settings'].includes(state.overlay)) render();
    }, 450);
  } else if (action === 'redis-fail') {
    state.redisDetection = 'error';
    render();
  } else if (action === 'save-settings' || action === 'save-settings-error') {
    const invalid = configValidate();
    if (invalid) {
      c.tab = ['maxRows', 'mcpTasks', 'approvalTtl'].includes(invalid)
        ? 'mcp'
        : ['planItems', 'planBytes', 'valueBytes', 'textBytes', 'previewRows', 'editorProgram', 'editorArgs'].includes(
            invalid,
          )
        ? 'editing'
        : 'general';
      c.message = ['保存失败，请修正字段错误。', 'Save failed; correct the field errors.'];
      render();
      document.querySelector(`[data-config-field="${invalid}"]`)?.focus();
      return;
    }
    const snapshot = configCopy(c.draft);
    c.settingsBusy = true;
    c.message = ['正在模拟保存全局设置…', 'Simulating global settings save…'];
    render();
    setTimeout(() => {
      c.settingsBusy = false;
      if (action === 'save-settings-error') {
        c.message = [
          '模拟全局保存失败；草稿保留，未发送。',
          'Simulated global save failed; draft retained, nothing was sent.',
        ];
      } else {
        c.saved = snapshot;
        c.message = [
          '全局设置已保存在本页示例中，未发送。',
          'Global settings saved in this page example; nothing was sent.',
        ];
      }
      if (state.overlay === 'app-settings') render();
    }, 450);
  } else if (action === 'reload-settings') {
    if (configDirty()) {
      c.confirmFocus = 'reload-settings';
      c.confirm = 'reload';
      render();
      document.querySelector('[data-config-action="cancel-confirm"]')?.focus();
    } else {
      c.draft = configCopy(c.saved);
      c.errors = {};
      c.message = ['已重新加载本页示例。', 'Reloaded the page example.'];
      render();
    }
  } else if (action === 'cancel-confirm') {
    configCancelConfirm();
  } else if (action === 'confirm-discard') {
    const close = c.confirm === 'close';
    c.confirm = '';
    c.draft = configCopy(c.saved);
    c.errors = {};
    if (close) {
      c.ruleDrafts = {};
      c.rules = c.profile ? configCopy(c.savedRules[c.profile]) : {};
      if (c.profile) c.policy = c.savedPolicies[c.profile];
      state.overlay = null;
    } else c.message = ['已重新加载本页示例。', 'Reloaded the page example.'];
    render();
  } else if (action === 'file-state') {
    c.fileState = button.dataset.value;
    render();
  } else if (action === 'file-select') {
    c.fileSelected = button.dataset.value;
    render();
  } else if (action === 'file-claim') {
    const file = configFiles.find(item => item.id === c.fileSelected);
    if (file && !file.active && !file.referenced) c.fileClaims[file.id] = file.version;
    render();
  } else if (action === 'file-cleanup') {
    const file = configFiles.find(item => item.id === c.fileSelected);
    if (file && !file.referenced && c.fileClaims[file.id] === file.version) {
      c.confirmFocus = 'file-cleanup';
      c.fileConfirm = configCopy(file);
      c.confirm = 'file';
      render();
      document.querySelector('[data-config-action="cancel-confirm"]')?.focus();
    }
  } else if (action === 'load-error') {
    c.busy = true;
    c.ruleState = 'loading';
    c.ruleMessage = ['正在模拟加载…', 'Simulating loading…'];
    render();
    setTimeout(() => {
      c.busy = false;
      c.ruleState = 'error';
      c.ruleMessage = ['模拟加载失败；未发送请求。', 'Simulated load failure; no request was sent.'];
      if (state.overlay === 'app-settings') render();
    }, 450);
  } else if (action === 'set-rule') {
    if (!c.profile.endsWith('redis') && !c.scope.trim()) {
      c.ruleMessage = ['请输入 MySQL 数据库名称。', 'Enter a MySQL database name.'];
      render();
      return;
    }
    c.rules[c.profile.endsWith('redis') ? '*' : c.scope] = c.level;
    c.ruleMessage = ['已修改本页示例，尚未保存。', 'Changed in this example; not yet saved.'];
    render();
  } else if (action === 'save-rules' || action === 'save-error') {
    c.busy = true;
    c.ruleMessage = ['正在模拟保存…', 'Simulating save…'];
    render();
    setTimeout(() => {
      c.busy = false;
      if (action === 'save-error') {
        c.ruleMessage = ['模拟保存失败；更改仍在示例中。', 'Simulated save failure; example changes remain.'];
      } else {
        c.savedRules[c.profile] = configCopy(c.rules);
        c.savedPolicies[c.profile] = c.policy;
        state.connectionApprovalPolicies[c.profile] = c.policy;
        c.ruleDrafts[c.profile] = { rules: configCopy(c.rules), scope: c.scope, level: c.level, policy: c.policy };
        c.ruleMessage = ['已保存在本页示例中，未发送。', 'Saved in this page example; nothing was sent.'];
      }
      if (state.overlay === 'app-settings') render();
    }, 450);
  }
});

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
  busy: false,
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
    h(configText('Redis DB 数量覆盖（留空自动探测）', 'Redis database count override (blank for detection)'))
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
    ? 'max="1000"'
    : key === 'responseBytes'
    ? 'max="1048576"'
    : '';
  const label = configText(zh, en);
  return configRow(
    label,
    `<input type="number" min="${
      key === 'responseBytes' ? '1024' : '1'
    }" ${max} step="1" data-config-field="${key}" aria-label="${h(label)}" value="${h(configPreview.draft[key])}" ${
      optional ? '' : 'required'
    }>`,
    !optional,
  );
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
function configLevelLabel(level) {
  return ({
    disabled: configText('禁用', 'Disabled'),
    read_only: configText('只读', 'Read only'),
    read_write: configText('读写', 'Read and write'),
  })[level];
}
function settingsPreview() {
  const c = configPreview, d = c.draft;
  const languageSelect = `<select id="language-preference" aria-label="${h(t('language'))}">${
    ['system', 'zh-CN', 'en'].map(value =>
      `<option value="${value}" ${languagePreference === value ? 'selected' : ''}>${
        h(t(value === 'system' ? 'system' : value === 'zh-CN' ? 'chinese' : 'english'))
      }</option>`
    ).join('')
  }</select>`;
  const general = `<section class="detail-section">${configRow(t('language'), languageSelect)}</section>
    <section class="detail-section"><h3>${h(configText('分页', 'Paging'))}</h3>${
    configNumber('mysqlRows', 'MySQL 表页行数', 'MySQL table rows per page')
  }${configNumber('sqlRows', 'SQL 页行数', 'SQL rows per page')}${
    configNumber('redisRows', 'Redis 值页行数', 'Redis value rows per page')
  }${configNumber('historyRows', '历史页行数', 'History rows per page')}${
    configNumber('archeryRows', 'Archery 表格页行数', 'Archery grid rows per page')
  }${configNumber('redisKeys', 'Redis 键批量', 'Redis key batch')}${
    configNumber('requestRows', 'Archery 请求行数', 'Archery request rows')
  }</section>
    <section class="detail-section"><h3>${h(configText('限制', 'Limits'))}</h3>${
    configNumber('responseBytes', '响应字节上限', 'Response byte limit')
  }${configNumber('previewBytes', '值预览字节数', 'Value preview bytes')}${
    configNumber('connectTimeout', '连接超时（秒）', 'Connect timeout (seconds)')
  }${configNumber('queryTimeout', '查询超时（秒）', 'Query timeout (seconds)')}${
    configNumber('countTimeout', '计数超时（秒）', 'Count timeout (seconds)')
  }${configNumber('resultIdle', '结果空闲时间（秒）', 'Result idle time (seconds)')}${
    configNumber('resultMemory', '客户端结果内存字节数', 'Result memory bytes per client')
  }</section>
    <section class="detail-section"><h3>${h(configText('历史', 'History'))}</h3>${
    configSwitch('cleanup', '自动清理历史', 'Automatically clean up history')
  }${configNumber('retentionDays', '保留天数（可留空）', 'Retention days (optional)', true)}${
    configNumber('maxEntries', '最大条数（可留空）', 'Maximum entries (optional)', true)
  }</section>
    <section class="detail-section config-servers">${serverList()}</section>`;
  const mcp = `<section class="detail-section"><h3>${h(configText('MCP 限制与审批', 'MCP limits and approval'))}</h3>${
    configNumber('maxRows', 'MCP 最大行数', 'Maximum MCP rows')
  }${configNumber('mcpTasks', 'MCP 并行任务数', 'Running tasks per MCP client')}${
    configNumber('approvalTtl', '审批有效期（秒）', 'Approval TTL (seconds)')
  }${
    configRow(
      configText('新连接默认审批策略', 'Default approval policy for new connections'),
      configPolicySelect(d.defaultPolicy, 'default', ['dangerous', 'all_writes']),
    )
  }</section>
    <section class="detail-section"><h3>${h(configText('通知', 'Notifications'))}</h3>${
    configSwitch('approvalNotice', '审批通知', 'Approval notifications')
  }${configSwitch('authNotice', '认证通知', 'Authentication notifications')}</section>
    <section class="detail-section"><h3>${h(configText('MCP 连接权限', 'MCP connection permissions'))}</h3>${
    configSwitch('fineGrained', '启用 MCP 细粒度权限', 'Enable fine-grained MCP permissions')
  }${
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
  }</p>${c.profile ? configRulesPreview() : ''}</section>`;
  return `<div class="config-preview"><p class="notice small">${
    h(configText(
      '配置与权限均为本页示例，不连接数据库，也不会发送或保存真实配置。语言偏好仍保存在浏览器中。',
      'Settings and permissions are examples on this page. No database is contacted and no real configuration is sent or saved. Language preference remains stored in your browser.',
    ))
  }</p>
    <div class="config-tabs" role="tablist" aria-label="${h(t('settings'))}">${
    [['general', configText('常规', 'General')], ['mcp', 'MCP']].map(([tab, label]) =>
      `<button type="button" role="tab" aria-selected="${
        c.tab === tab
      }" aria-controls="config-panel" data-config-action="tab" data-config-tab="${tab}">${h(label)}</button>`
    ).join('')
  }</div>
    <div id="config-panel" role="tabpanel" aria-label="${
    h(c.tab === 'general' ? configText('常规', 'General') : 'MCP')
  }">${c.tab === 'general' ? general : mcp}</div>
    <p role="status" class="muted small">${h(configMessage(c.message))}</p></div>`;
}
function settingsPreviewFooter() {
  return `<button type="button" class="button" data-config-action="reload-settings">${
    h(configText('重新加载示例设置', 'Reload example settings'))
  }</button><span class="spacer"></span><button type="button" class="button primary" data-config-action="save-settings">${
    h(configText('保存示例设置', 'Save example settings'))
  }</button>`;
}
function configRulesPreview() {
  const c = configPreview, redis = c.profile.endsWith('redis');
  const rows = Object.entries(c.rules).map(([scope, level]) =>
    configRow(
      redis && scope === '*' ? configText('Redis 整个实例', 'Entire Redis instance') : scope,
      `<span>${h(configLevelLabel(level))}</span>`,
    )
  ).join('');
  return `${configRow(configText('连接审批策略', 'Connection approval policy'), configPolicySelect(c.policy, 'rule'))}${
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
      }>${
        h(configText('应用权限', 'Apply permission'))
      }</button><button type="button" class="button" data-config-action="save-rules" ${c.busy ? 'disabled' : ''}>${
        h(configText('保存示例权限', 'Save example permissions'))
      }</button><button type="button" class="button" data-config-action="save-error" ${c.busy ? 'disabled' : ''}>${
        h(configText('模拟保存失败', 'Simulate save failure'))
      }</button></div>`
      : ''
  }`;
}
document.addEventListener('input', event => {
  const field = event.target.dataset.configField;
  if (field && field !== 'scope' && event.target.type !== 'checkbox') {
    configPreview.draft[field] = event.target.value;
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
    if (c.profile) c.ruleDrafts[c.profile] = { rules: configCopy(c.rules), scope: c.scope, level: c.level };
    c.profile = target.value;
    if (c.profile) {
      const draft = c.ruleDrafts[c.profile];
      c.rules = configCopy(draft ? draft.rules : c.savedRules[c.profile]);
      c.scope = draft ? draft.scope : '';
      c.level = draft ? draft.level : (c.profile.endsWith('redis') ? c.rules['*'] : null) || 'read_only';
      c.policy = state.connectionApprovalPolicies[c.profile];
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
      state.connectionApprovalPolicies[c.profile] = target.value;
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
    if (field === 'fineGrained') render();
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
  } else if (action === 'save-settings') {
    const invalid = document.querySelector('.config-preview input[type="number"]:invalid');
    if (invalid) {
      invalid.reportValidity();
      return;
    }
    c.saved = configCopy(c.draft);
    c.message = ['已保存在本页示例中，未发送。', 'Saved in this page example; nothing was sent.'];
    render();
  } else if (action === 'reload-settings') {
    c.draft = configCopy(c.saved);
    c.message = ['已重新加载本页示例。', 'Reloaded the page example.'];
    render();
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
        c.ruleMessage = ['已保存在本页示例中，未发送。', 'Saved in this page example; nothing was sent.'];
      }
      if (state.overlay === 'app-settings') render();
    }, 450);
  }
});

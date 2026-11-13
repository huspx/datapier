'use strict';

// 本文件只模拟界面状态；不访问数据库、文件或外部进程。
const advancedText = (zh, en) => language === 'zh-CN' ? zh : en;
const advancedEscape = value => h(String(value));
const advancedButton = (action, label, disabled = false) =>
  `<button type="button" class="button" data-preview-action="${action}"${disabled ? ' disabled' : ''}>${
    advancedEscape(label)
  }</button>`;
const advancedSelect = (name, label, options, value, disabled = false) =>
  `<label>${advancedEscape(label)}<select data-preview-field="${name}"${disabled ? ' disabled' : ''}>${
    options.map(([key, title]) =>
      `<option value="${key}"${key === value ? ' selected' : ''}>${advancedEscape(title)}</option>`
    ).join('')
  }</select></label>`;
const advancedSwitch = (name, label, checked, disabled = false) =>
  `<label class="advanced-switch"><span>${
    advancedEscape(label)
  }</span><input type="checkbox" role="switch" data-preview-field="${name}"${checked ? ' checked' : ''}${
    disabled ? ' disabled' : ''
  }></label>`;
const advancedInput = (name, label, value, disabled = false) =>
  `<label>${advancedEscape(label)}<input data-preview-field="${name}" value="${advancedEscape(value)}"${
    disabled ? ' disabled' : ''
  }></label>`;
const advancedNotice = () =>
  `<p class="notice">${
    advancedText(
      '交互设计示例：不会连接数据库、访问文件或启动编辑器。',
      'Interactive design sample: no database, file or editor access.',
    )
  }</p>`;

// Build comparisons from the same in-memory rows and keys used by the workspace.
function replacementContext(s) {
  if (s.source === 'mysql') {
    const matching = currentRows();
    const saved = state.rowFilter;
    let page;
    try {
      state.rowFilter = '';
      page = resultPageSlice(currentRows(), `mysql:${state.database}.${state.table}`);
    } finally {
      state.rowFilter = saved;
    }
    const visible = new Set(matching.map(rowId));
    const rows = s.scope === 'selected'
      ? matching.filter(row => state.selected.has(rowId(row)))
      : s.scope === 'page'
      ? page.filter(row => visible.has(rowId(row)))
      : matching;
    return {
      database: state.database,
      table: state.table,
      rows: rows.map(row => ({ ...row, identity: rowId(row) })),
      selection: [...state.selected],
      filters: state.filters[`${state.database}.${state.table}`],
      rowFilter: state.rowFilter,
      page: s.scope === 'page' ? rows.map(rowId) : null,
    };
  }
  const keys = state.selectedKeys.size ? [...state.selectedKeys] : keySamples.filter(key => key.name === state.key);
  return {
    db: state.redisDb,
    keys: keys.map(key => ({ ...key, rows: key.rows?.map(row => [...row]), bytes: key.bytes ? [...key.bytes] : null })),
  };
}

function replacementPlan(s) {
  const context = replacementContext(s);
  const criteria = {
    source: s.source,
    scope: s.scope,
    kind: s.kind,
    find: s.find,
    replace: s.replace,
    regex: s.regex,
    sensitive: s.sensitive,
    multiline: s.multiline,
    dotall: s.dotall,
    columns: [...s.columns],
    roles: [...s.roles],
  };
  const pattern = s.regex ? s.find : s.find.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const expression = new RegExp(
    pattern,
    `g${s.sensitive ? '' : 'i'}${s.regex && s.multiline ? 'm' : ''}${s.regex && s.dotall ? 's' : ''}`,
  );
  const items = [];
  let scanned = 0, binary = 0;
  const add = (item, before) => {
    scanned++;
    if (typeof before !== 'string') return;
    expression.lastIndex = 0;
    if (!expression.test(before)) return;
    expression.lastIndex = 0;
    const after = before.replace(expression, s.regex ? s.replace : () => s.replace);
    if (after !== before) items.push({ ...item, before, after, included: true, collision: false });
  };
  if (s.source === 'mysql') {
    for (const row of context.rows) {
      for (const column of s.columns) {
        if (Object.hasOwn(row, column)) {
          add(
            { identity: String(row.identity), identityColumn: row.id !== undefined ? 'id' : 'day', column },
            row[column],
          );
        }
      }
    }
  } else {
    const types = { string: 'STR', hash: 'HASH', list: 'LIST', set: 'SET', zset: 'ZSET' };
    for (const key of context.keys.filter(key => key.type === types[s.kind])) {
      if (s.roles.includes('key')) add({ key: key.name, role: 'key', ttl: key.ttl }, key.name);
      if (s.kind === 'string' && s.roles.includes('value')) {
        if (key.bytes) {
          scanned++;
          binary++;
        } else add({ key: key.name, role: 'value', ttl: key.ttl }, key.value);
      }
      for (const [index, row] of (key.rows || []).entries()) {
        if (s.kind === 'hash') {
          if (s.roles.includes('field')) {
            add({ key: key.name, role: 'field', field: row[0], value: row[1], ttl: key.ttl }, row[0]);
          }
          if (s.roles.includes('value')) add({ key: key.name, role: 'value', field: row[0], ttl: key.ttl }, row[1]);
        } else if (s.kind === 'list' && s.roles.includes('value')) {
          add({ key: key.name, role: 'value', index: row[0], ttl: key.ttl }, row[1]);
        } else if ((s.kind === 'set' || s.kind === 'zset') && s.roles.includes('member')) {
          add({ key: key.name, role: 'member', score: row[1], ttl: key.ttl }, row[0]);
        }
      }
    }
    for (const item of items) {
      const key = context.keys.find(candidate => candidate.name === item.key);
      if (item.role === 'key') item.collision = keySamples.some(candidate => candidate.name === item.after);
      else if (item.role === 'field' || item.role === 'member') {
        item.collision = key.rows.some(row => row[0] === item.after);
      }
      if (
        ['key', 'field', 'member'].includes(item.role)
        && items.some(other =>
          other !== item && other.role === item.role && (item.role === 'key' || other.key === item.key)
          && other.after === item.after
        )
      ) item.collision = true;
      if (item.collision) item.included = false;
    }
  }
  const expectedType = { string: 'STR', hash: 'HASH', list: 'LIST', set: 'SET', zset: 'ZSET' }[s.kind];
  const inapplicable = s.source === 'redis' ? context.keys.filter(key => key.type !== expectedType) : [];
  return { context, criteria, items, scanned, binary, inapplicable, signature: JSON.stringify(context) };
}

function replacementCommands(plan, item) {
  if (!item || item.collision || !item.included) return [];
  const { criteria: c, context } = plan;
  if (c.source === 'mysql') {
    const quote = name => '`' + name.replace(/`/g, '``') + '`';
    return [{
      text: `UPDATE ${quote(context.database)}.${quote(context.table)} SET ${quote(item.column)} = ? WHERE ${
        quote(item.identityColumn)
      } = ? AND BINARY ${quote(item.column)} = BINARY ?`,
      args: [item.after, item.identity, item.before],
    }];
  }
  const cmd = (text, ...args) => ({ text, args });
  if (item.role === 'key') return [cmd('RENAMENX', item.key, item.after)];
  if (c.kind === 'string') return [cmd('SET', item.key, item.after, 'KEEPTTL')];
  if (c.kind === 'hash' && item.role === 'value') return [cmd('HSET', item.key, item.field, item.after)];
  if (c.kind === 'hash') return [cmd('HSETNX', item.key, item.after, item.value), cmd('HDEL', item.key, item.before)];
  if (c.kind === 'list') return [cmd('LSET', item.key, item.index, item.after)];
  if (c.kind === 'set') return [cmd('SREM', item.key, item.before), cmd('SADD', item.key, item.after)];
  return [cmd('ZREM', item.key, item.before), cmd('ZADD', item.key, item.score, item.after)];
}

function replacementPreview(info) {
  const t = advancedText;
  const initialKey = [...state.selectedKeys][0] || keySamples.find(key => key.name === state.key);
  const initialKind = { STR: 'string', HASH: 'hash', LIST: 'list', SET: 'set', ZSET: 'zset' }[initialKey?.type]
    || 'string';
  const initialColumns = Object.keys(databases[state.database]?.[state.table]?.[0] || {}).filter(column =>
    column !== 'id' && column !== 'day'
  );
  const s = state.replacementPreview ||= {
    source: state.provider === 'redis' ? 'redis' : 'mysql',
    scope: 'selected',
    kind: initialKind,
    phase: 'editing',
    find: 'pending',
    replace: 'paid',
    columns: initialColumns.includes('status') ? ['status'] : initialColumns.slice(0, 1),
    roles: [initialKind === 'set' || initialKind === 'zset' ? 'member' : 'value'],
    regex: false,
    sensitive: false,
    multiline: false,
    dotall: false,
    page: 1,
    pageSize: '1',
    unknownTotal: false,
    chunk: 1,
    plan: null,
    error: '',
  };
  if (s.plan && s.phase === 'frozen' && JSON.stringify(replacementContext(s)) !== s.plan.signature) s.phase = 'stale';
  const editable = s.phase === 'editing' || s.phase === 'restored';
  const frozen = !editable;
  const phaseOptions = [
    ['editing', t('编辑条件', 'Edit criteria')],
    ['frozen', t('冻结预览', 'Frozen preview')],
    ['stale', t('计划失效', 'Plan invalidated')],
    ['restored', t('恢复草稿', 'Restore draft')],
    ['incomplete', t('收集不完整', 'Incomplete collection')],
    ['running', t('提交中', 'Submitting')],
    ['partial', t('部分生效', 'Partially applied')],
    ['conflict', t('执行基线冲突', 'Execution baseline conflict')],
    ['unknown', t('结果未知', 'Unknown result')],
    ['readonly', t('只读', 'Read only')],
  ];
  const roles = {
    string: [['value', 'String value'], ['key', 'Key']],
    hash: [['value', 'Hash value'], ['field', 'Hash field'], ['key', 'Key']],
    list: [['value', 'List element'], ['key', 'Key']],
    set: [['member', 'Set member'], ['key', 'Key']],
    zset: [['member', 'ZSet member'], ['key', 'Key']],
  };
  const choose = (name, values, chosen) =>
    values.map(([key, label]) =>
      `<label><input type="checkbox" data-preview-group="${name}" value="${advancedEscape(key)}"${
        chosen.includes(key) ? ' checked' : ''
      }${frozen ? ' disabled' : ''}> ${advancedEscape(label)}</label>`
    ).join(' ');
  const targetContext = frozen && s.plan ? s.plan.context : replacementContext(s);
  const columns = Object.keys(
    databases[targetContext.database || state.database]?.[targetContext.table || state.table]?.[0] || {},
  ).filter(column => column !== 'id' && column !== 'day');
  const target = s.source === 'mysql'
    ? `<dl class="facts">${fact(t('目标', 'Target'), `${targetContext.database}.${targetContext.table}`)}${
      fact(t('范围内行数', 'Rows in scope'), String(targetContext.rows.length))
    }${
      fact(
        t('范围内行身份', 'Row identities in scope'),
        targetContext.rows.map(row => String(row.identity)).join(', ') || '—',
      )
    }${
      fact(
        t('筛选条件', 'Filters'),
        JSON.stringify(targetContext.filters || {})
          + ` · ${t('行筛选', 'Row filter')}: ${targetContext.rowFilter || '—'}`,
      )
    }</dl>${
      advancedSelect(
        'scope',
        t('范围', 'Scope'),
        [['selected', t('选中行', 'Selected rows')], ['page', t('当前页', 'Current page')], [
          'filter',
          t('当前筛选结果', 'Filtered results'),
        ]],
        s.scope,
        frozen,
      )
    }<fieldset><legend>${t('替换列', 'Columns')}</legend>${
      choose('columns', columns.map(column => [column, column]), s.columns)
    }</fieldset>`
    : `<dl class="facts">${fact(t('目标', 'Target'), `Redis DB ${targetContext.db}`)}${
      fact(t('选集', 'Selection'), targetContext.keys.map(key => key.name).join(', ') || '—')
    }</dl>${
      advancedSelect(
        'kind',
        t('类型', 'Type'),
        [['string', 'String'], ['hash', 'Hash'], ['list', 'List'], ['set', 'Set'], ['zset', 'ZSet']],
        s.kind,
        frozen,
      )
    }<fieldset><legend>${t('替换角色', 'Replacement roles')}</legend>${
      choose('roles', roles[s.kind], s.roles)
    }</fieldset>`;
  const flags = [['regex', t('正则表达式', 'Regular expression')], ['sensitive', t('区分大小写', 'Case sensitive')], [
    'multiline',
    t('多行', 'Multiline'),
  ], ['dotall', t('点匹配换行', 'Dot matches newline')]].map(([key, label]) =>
    advancedSwitch(key, label, s[key], frozen || (!s.regex && (key === 'multiline' || key === 'dotall')))
  ).join('') + (s.regex
    ? `<p class="muted">${
      t(
        '本地示例使用浏览器 JavaScript 正则语义；实际支持范围以原生引擎校验为准。',
        'This local sample uses browser JavaScript regular expressions; actual supported patterns are validated by the native engine.',
      )
    }</p>`
    : '');
  const messages = {
    frozen: t(
      '目标、条件及基线已冻结；丢弃计划后才能修改。',
      'Target, criteria and baseline are frozen. Discard the plan to edit.',
    ),
    stale: t('目标或选集变化，计划已失效；丢弃后重新准备。', 'Target or selection changed. Discard and prepare again.'),
    restored: t('恢复草稿后须重新准备基线。', 'Prepare a new baseline after restoring the draft.'),
    incomplete: t(
      '收集未完成，不能提交已收集的前缀。',
      'Collection is incomplete; the collected prefix cannot be submitted.',
    ),
    running: t(
      '模拟提交中；停止等待不代表远端取消。',
      'Simulated submission; stopping the wait does not cancel remote work.',
    ),
    partial: t('模拟首错停止，请核对后恢复。', 'Simulated stop at first error; inspect before recovery.'),
    unknown: t(
      '结果未知，先核对目标和执行记录；不会自动重放。',
      'Result unknown; inspect the target and execution record. No automatic replay.',
    ),
    readonly: t('只读连接不能准备或提交。', 'Read-only connections cannot prepare or submit.'),
  };
  const plan = s.plan, items = plan?.items || [];
  const pageSize = Number(s.pageSize || 1);
  const pages = Math.max(1, Math.ceil(items.length / pageSize));
  s.page = Math.max(1, Math.min(s.page, pages));
  const pageStart = (s.page - 1) * pageSize;
  const item = items[pageStart];
  const commands = plan ? replacementCommands(plan, item) : [];
  const row = items.slice(pageStart, pageStart + pageSize).map((entry, offset) => {
    const identity = entry.identity
      ? `${entry.identity} · ${entry.column}`
      : `${entry.key} · ${entry.role}${
        entry.field !== undefined ? ` · ${entry.field}` : entry.index !== undefined ? ` · ${entry.index}` : ''
      } · TTL ${entry.ttl}${entry.score !== undefined ? ` · score ${entry.score}` : ''}`;
    return `<tr><td>${advancedEscape(identity)}</td><td>${advancedEscape(entry.before)}</td><td>${
      advancedEscape(entry.after)
    }</td><td>${
      entry.collision
        ? t('碰撞，不能包含', 'Collision; cannot include')
        : `<label><input type="checkbox" data-preview-include="${pageStart + offset}"${
          entry.included ? ' checked' : ''
        }${s.phase !== 'frozen' ? ' disabled' : ''}> ${
          entry.included ? t('包含', 'Included') : t('已排除', 'Excluded')
        }</label>`
    }</td></tr>`;
  }).join('') || `<tr><td colspan="4">${t('没有匹配且发生变化的项目', 'No matching changed items')}</td></tr>`;
  const commandMarkup = commands.map((command, index) =>
    `<p><code>${advancedEscape(command.text)}</code></p>${
      simpleTable(
        [t('序号', 'Position'), t('值', 'Value')],
        command.args.map((value, position) => [`${index + 1}.${position + 1}`, String(value)]),
      )
    }`
  ).join('');
  const preview = plan && !['editing', 'restored', 'readonly'].includes(s.phase)
    ? `<section class="detail-section"><h3>${t('冻结项比较', 'Frozen comparison')}</h3><p>${
      t('扫描', 'Scanned')
    } ${plan.scanned} · ${t('变化', 'Changed')} ${items.length} · ${
      t('非 UTF-8 跳过', 'Non-UTF-8 skipped')
    } ${plan.binary} · ${t('碰撞', 'Collisions')} ${items.filter(entry => entry.collision).length} · ${
      t('包含', 'Included')
    } ${items.filter(entry => entry.included).length} · ${t('不适用', 'Inapplicable')} ${plan.inapplicable.length}</p>${
      plan.inapplicable.length
        ? `<p>${t('未参与（类型不匹配）', 'Not included (type mismatch)')}: ${
          advancedEscape(plan.inapplicable.map(key => `${key.name} (${key.type})`).join(', '))
        }</p>`
        : ''
    }<div class="schema-table-scroll"><table class="schema-table"><thead><tr><th>${t('身份', 'Identity')}</th><th>${
      t('原值', 'Before')
    }</th><th>${t('新值', 'After')}</th><th>${
      t('包含', 'Include')
    }</th></tr></thead><tbody>${row}</tbody></table></div><div class="fields">${
      advancedSelect('pageSize', t('每页数量', 'Page size'), [['1', '1'], ['2', '2'], ['5', '5']], String(pageSize))
    }${advancedSwitch('unknownTotal', t('模拟总数未知', 'Simulate unknown total'), !!s.unknownTotal)}</div><p>${
      t('第', 'Page')
    } ${s.page}${s.unknownTotal ? ` · ${t('总页数未知', 'Total pages unknown')}` : ` / ${pages}`} ${
      advancedButton('replace-prev', t('上一页', 'Previous'), s.page === 1)
    } ${advancedButton('replace-next', t('下一页', 'Next'), s.page === pages)}</p>${
      item
        ? `<details><summary>${t('完整值（分块）', 'Full value (chunks)')}</summary><p>${
          t('块', 'Chunk')
        } ${s.chunk} · ${advancedEscape(item.before.slice((s.chunk - 1) * 64, s.chunk * 64))}</p>${
          advancedButton('replace-chunk', t('下一块', 'Next chunk'), item.before.length <= 64)
        }</details>`
        : ''
    }</section><section class="detail-section"><h3>${
      t('当前项命令与有序参数（未发送）', 'Current item commands and ordered arguments (not sent)')
    }</h3>${commandMarkup || `<p>${t('此项没有可提交的命令。', 'No command for this item.')}</p>`}${
      plan.criteria.source === 'redis' && item
        ? `<p>${
          t(
            '示例命令仅展示参数；执行前仍需原子校验基线与碰撞，并保留适用的 TTL、field 期限及 score。',
            'Commands show arguments only; execution still requires atomic baseline and collision checks and preservation of applicable TTL, field expiry and score.',
          )
        }</p>`
        : ''
    }</section>`
    : '';
  const result = ['running', 'partial', 'unknown', 'conflict'].includes(s.phase)
    ? `<section class="detail-section"><h3>${t('执行结果示例', 'Sample execution result')}</h3><p>${
      t('模拟状态；没有执行命令。', 'Simulated state; no commands were executed.')
    } ${t('计划包含', 'Plan includes')} ${items.filter(entry => entry.included).length} · ${
      t('碰撞或排除', 'Colliding or excluded')
    } ${items.filter(entry => !entry.included).length}</p>${
      items.length
        ? simpleTable(
          [t('项目', 'Item'), t('状态', 'Status'), t('说明', 'Details')],
          items.map((entry, index) => [
            entry.identity || entry.key,
            !entry.included
              ? t('未执行', 'Not executed')
              : s.phase === 'unknown'
              ? t('结果未知', 'Unknown')
              : s.phase === 'running'
              ? t('等待中', 'Waiting')
              : s.phase === 'conflict'
              ? t('冲突', 'Conflict')
              : index === 0
              ? t('已生效', 'Applied')
              : index === 1
              ? t('失败', 'Failed')
              : t('未执行', 'Not executed'),
            entry.collision
              ? t('碰撞', 'Collision')
              : s.phase === 'conflict'
              ? t(
                '模拟 CAS / WATCH 基线冲突；未执行，需重新读取基线',
                'Simulated CAS / WATCH baseline conflict; not executed. Re-read the baseline.',
              )
              : s.phase === 'partial' && index === 1
              ? t('模拟错误；首错停止', 'Simulated error; stopped at first failure')
              : '—',
          ]),
        )
        : `<p>${t('没有可显示的项目。', 'No items to display.')}</p>`
    }</section>`
    : '';
  return modal(
    t('批量查找替换', 'Batch find and replace'),
    `${advancedNotice()}<div class="advanced-preview"><div class="fields">${
      advancedSelect('replace-phase', t('示例状态', 'Sample state'), phaseOptions, s.phase)
    }${
      advancedSelect('source', t('来源', 'Source'), [['mysql', 'MySQL'], ['redis', 'Redis']], s.source, frozen)
    }</div>${target}<section class="detail-section"><h3>${
      t('替换条件', 'Replacement criteria')
    }</h3><div class="fields">${advancedInput('find', t('查找', 'Find'), s.find, frozen)}${
      advancedInput('replace', t('替换为', 'Replace with'), s.replace, frozen)
    }</div>${flags}</section>${messages[s.phase] ? `<p class="notice">${messages[s.phase]}</p>` : ''}${
      s.error ? `<p role="alert">${advancedEscape(s.error)}</p>` : ''
    }${preview}${result}${
      plan && s.phase === 'frozen'
        ? `<div class="design-actions">${
          advancedButton(
            'replace-conflict',
            t('模拟执行时 CAS / WATCH 冲突', 'Simulate execution CAS / WATCH conflict'),
            !items.some(entry => entry.included),
          )
        }</div>`
        : ''
    }${
      s.phase === 'conflict'
        ? `<div class="design-actions">${
          advancedButton('replace-rebaseline', t('重新读取基线并准备', 'Re-read baseline and prepare'))
        }</div>`
        : ''
    }</div>`,
    `${
      advancedButton('replace-edit', t('丢弃计划并修改', 'Discard plan and edit'), editable && !plan)
    }<span class="spacer"></span>${
      advancedButton(
        'replace-prepare',
        t('准备并预览', 'Prepare and preview'),
        !editable || !s.find || !(s.source === 'mysql'
          ? s.columns.some(column => columns.includes(column))
          : s.roles.length)
          || state.connectionModes[s.source],
      )
    }<button class="button primary" disabled>${t('提交', 'Submit')}</button><span class="muted">${
      t('设计示例不可提交', 'Submission unavailable in design sample')
    }</span>`,
  );
}

function formatAdvancedJson(source, format) {
  const token =
    /[ \t\r\n]+|"(?:\\.|[^"\\])*"|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?|true|false|null|[{}\[\],:]/gy;
  const tokens = [];
  let offset = 0;
  while (offset < source.length) {
    token.lastIndex = offset;
    const match = token.exec(source);
    if (!match) throw new SyntaxError('Invalid JSON');
    offset = token.lastIndex;
    if (!/^[ \t\r\n]+$/.test(match[0])) tokens.push(match[0]);
  }
  // Validate structure and strings without converting the original numbers to Number.
  JSON.parse(tokens.map(part => /^-?[0-9]/.test(part) ? '0' : part).join(' '));
  if (!format) return source;
  let result = '', depth = 0;
  const indent = () => '  '.repeat(depth);
  for (let i = 0; i < tokens.length; i++) {
    const part = tokens[i], previous = tokens[i - 1], next = tokens[i + 1];
    if (part === '{' || part === '[') {
      result += part;
      depth++;
      if (next !== '}' && next !== ']') result += '\n' + indent();
    } else if (part === '}' || part === ']') {
      depth--;
      if (previous !== '{' && previous !== '[') result += '\n' + indent();
      result += part;
    } else if (part === ',') result += ',\n' + indent();
    else if (part === ':') result += ': ';
    else result += part;
  }
  return result;
}

function advancedValuePreview(info) {
  const t = advancedText;
  const s = state.valuePreview ||= {
    phase: 'normal',
    mode: 'text',
    value: '{"status":"pending"}',
    nullType: 'text',
    chunk: 1,
    overwrite: false,
    path: '/tmp/datapier-edit-example.txt',
    validation: '',
    file: 'none',
    confirm: false,
    dirty: false,
  };
  const modes = [['text', t('文本', 'Text')], ['hex', 'Hex'], ['json', 'JSON'], ['xml', 'XML'], [
    'image',
    t('图片', 'Image'),
  ]];
  const phases = [
    ['normal', t('普通值', 'Normal value')],
    ['null', 'NULL'],
    ['readonly', t('只读', 'Read only')],
    ['large', t('大值分块', 'Large value chunks')],
    ['image-loading', t('图片生成中', 'Generating image')],
    ['image-error', t('图片失败', 'Image failure')],
    ['image-ready', t('图片结果', 'Image result')],
    ['dirty-confirm', t('未应用关闭确认', 'Discard confirmation')],
    ['file-ready', t('外部文件已准备', 'External file prepared')],
    ['file-running', t('终端编辑器运行中', 'Terminal editor running')],
    ['file-returned', t('已返回值编辑器', 'Returned to value editor')],
    ['file-failed', t('编辑器启动失败', 'Editor launch failed')],
    ['file-read', t('已读取修改', 'Changes read')],
    ['file-kept', t('已保留文件', 'File retained')],
    ['file-cleanup', t('清理确认', 'Cleanup confirmation')],
  ];
  const readonly = s.phase === 'readonly' || s.phase === 'large';
  if (!s.bytes) s.bytes = new TextEncoder().encode(s.value);
  const windowSize = s.phase === 'large' ? 64 : Math.max(1, s.bytes.length);
  const start = (s.chunk - 1) * windowSize;
  const end = Math.min(start + windowSize, s.bytes.length);
  const hex = Array.from(s.bytes.slice(start, end), byte => byte.toString(16).padStart(2, '0')).join(' ');
  const editableHex = s.mode === 'hex' && s.phase !== 'readonly' && !state.cellValueTarget?.readonly;
  const filePhase = s.phase.startsWith('file-');
  const image = s.phase === 'image-loading'
    ? t('正在生成预览…', 'Generating preview…')
    : s.phase === 'image-error'
    ? t('生成失败；原值保留，可重试。', 'Generation failed; original value retained. Retry is available.')
    : s.phase === 'image-ready'
    ? t(
      '示例图片结果：SVG；宿主会缓存生成的预览。',
      'Sample image result: SVG; generated previews are cached by the host.',
    )
    : t('图片预览需显式生成；宿主会缓存预览。', 'Image preview is generated explicitly and cached by the host.');
  const validation = s.validation ? `<p role="status">${advancedEscape(s.validation)}</p>` : '';
  const editor = s.mode === 'image'
    ? `<p>${image}</p>${
      s.phase === 'image-ready'
        ? `<svg role="img" aria-label="${
          t('示例图片', 'Sample image')
        }" width="140" height="80" viewBox="0 0 140 80"><rect width="140" height="80" fill="#d9e8f1"/><circle cx="42" cy="32" r="14" fill="#688ca5"/><path d="M0 80 L55 37 L85 65 L108 47 L140 80 Z" fill="#8ab39a"/></svg>`
        : ''
    }${advancedButton('value-image', t('生成示例预览', 'Generate sample preview'))}`
    : `<label class="design-value-label">${s.mode === 'hex' ? 'Hex' : t('值', 'Value')}<textarea data-preview-field="${
      s.mode === 'hex' ? 'hexWindow' : 'value'
    }" rows="7"${readonly && !editableHex ? ' readonly' : ''}>${
      advancedEscape(
        s.mode === 'hex'
          ? (s.hexDraft ?? hex)
          : s.phase === 'large'
          ? new TextDecoder().decode(s.bytes.slice(start, end))
          : s.value,
      )
    }</textarea></label>${
      ['json', 'xml'].includes(s.mode)
        ? `${advancedButton('value-validate', t('校验', 'Validate'))} ${
          advancedButton('value-format', t('格式化', 'Format'), readonly)
        }`
        : ''
    }${validation}`;
  const fileStatus = {
    'file-ready': t(
      '文件已准备；启动或读取前请核对路径。',
      'File prepared; inspect the path before launching or reading.',
    ),
    'file-running': t(
      '终端编辑器仍运行；返回后显式读取修改。',
      'Terminal editor is still running; explicitly read changes after returning.',
    ),
    'file-returned': t('已返回；必须显式读取修改。', 'Returned; changes must be read explicitly.'),
    'file-failed': t('启动失败，文件保留；可重试或手动编辑。', 'Launch failed; file retained. Retry or edit manually.'),
    'file-read': t(
      '修改已读取到值会话；加入数据草稿仍是下一步。',
      'Changes read into the value session; staging remains a separate step.',
    ),
    'file-kept': t('文件已保留，可在设置中管理遗留文件。', 'File retained; manage it later in settings.'),
    'file-cleanup': t(
      '确认清理以下文件？清理后无法从此会话恢复；不会关闭外部编辑器。',
      'Clean up this file? It cannot be recovered from this session; the external editor will not be closed.',
    ),
  };
  const confirm = s.phase === 'dirty-confirm'
    ? `<section class="detail-section"><h3>${t('丢弃未应用修改？', 'Discard unapplied changes?')}</h3><p>${
      t('当前输入尚未应用到值会话。', 'Current input has not been applied to the value session.')
    }</p>${advancedButton('value-continue', t('继续编辑', 'Continue editing'))} ${
      advancedButton('value-discard', t('丢弃修改', 'Discard changes'))
    }</section>`
    : '';
  return modal(
    t('高级值编辑', 'Advanced value editor'),
    `${advancedNotice()}<div class="advanced-preview">${
      advancedSelect('value-phase', t('示例状态', 'Sample state'), phases, s.phase, !!s.cleanupTarget)
    }<dl class="facts">${fact(t('连接 / 资源', 'Connection / resource'), 'Local MySQL · commerce.orders')}${
      fact(t('行 / 字段', 'Row / column'), 'id = 10422 · metadata')
    }${
      fact(
        t('会话状态', 'Session state'),
        s.phase === 'null' ? 'NULL' : s.dirty ? t('有未应用修改', 'Unapplied changes') : t('未加入草稿', 'Not staged'),
      )
    }</dl>${
      s.phase === 'null'
        ? `<p>${
          t(
            'NULL 与空字符串和空字节不同。选择新值类型后编辑。',
            'NULL differs from empty text and empty bytes. Choose the new value type.',
          )
        }</p>${
          advancedSelect('nullType', t('新值类型', 'New value type'), [['text', t('文本', 'Text')], [
            'binary',
            t('二进制', 'Binary'),
          ], ['json', 'JSON']], s.nullType)
        }`
        : ''
    }${
      readonly
        ? `<p class="notice">${
          s.phase === 'large'
            ? t(
              '这是生成的大值示例。文本超过编辑上限，只读查看；Hex 可编辑当前字节窗口，其他字节保留。',
              'This is a generated large-value sample. Text exceeds the editing limit and is read-only; Hex edits the current byte window and preserves all other bytes.',
            )
            : t(
              '只读连接：可查看和导出，不能修改。',
              'Read-only connection: inspect and export, but editing is unavailable.',
            )
        }</p>`
        : ''
    }<div class="config-tabs design-tabs" role="tablist" aria-label="${t('值模式', 'Value mode')}">${
      modes.map(([id, label]) =>
        `<button type="button" role="tab" aria-selected="${s.mode === id}" tabindex="${
          s.mode === id ? 0 : -1
        }" data-preview-mode="${id}">${label}</button>`
      ).join('')
    }</div><section class="detail-section" role="tabpanel">${editor}${
      s.phase === 'large'
        ? `<p>${t('字节范围（含首尾）', 'Byte range (inclusive)')} ${start}–${
          Math.max(start, end - 1)
        } / ${s.bytes.length} · ${t('块', 'Chunk')} ${s.chunk}</p>${
          advancedButton('value-prev', t('上一块', 'Previous chunk'), s.chunk === 1)
        } ${advancedButton('value-next', t('下一块', 'Next chunk'), end >= s.bytes.length)}`
        : ''
    }<div class="design-actions">${
      advancedButton(
        'value-apply',
        t('应用到值会话', 'Apply to value session'),
        (readonly && !editableHex) || !s.dirty || !!s.hexError,
      )
    } ${
      advancedButton('value-null', t('设为 NULL', 'Set NULL'), readonly)
    }</div></section>${confirm}<section class="detail-section"><h3>${
      t('文件与外部编辑', 'Files and external editing')
    }</h3>${advancedInput('path', t('文件路径', 'File path'), s.path, !!s.cleanupTarget)}${
      advancedSwitch('overwrite', t('允许覆盖已有文件', 'Overwrite existing file'), s.overwrite, !!s.cleanupTarget)
    }<p class="muted">${
      t(
        '设计示例禁用真实导入、导出、准备文件和启动进程；路径仅供展示。',
        'Real import, export, file preparation and process launch are disabled in this sample; the path is illustrative.',
      )
    }</p><div class="design-actions"><button class="button" disabled>${
      t('导入', 'Import')
    }</button><button class="button" disabled>${t('导出', 'Export')}</button><button class="button" disabled>${
      t('准备外部编辑', 'Prepare external edit')
    }</button></div>${
      filePhase
        ? `<p>${fileStatus[s.phase]}</p><p><code>${
          advancedEscape(s.demoFile?.path || s.path)
        }</code></p><div class="design-actions"><button class="button" disabled>${
          t('启动编辑器', 'Launch editor')
        }</button><button class="button" disabled>${
          t('读取修改', 'Read changes')
        }</button><button class="button" disabled>${
          t('保留文件', 'Keep file')
        }</button><button class="button danger" disabled>${t('结束并清理', 'Finish and clean up')}</button></div>`
        : ''
    }</section></div>`,
    `<span class="spacer"></span><button class="button primary" disabled>${
      t('加入草稿', 'Stage draft')
    }</button><span class="muted">${t('设计示例不可提交', 'Staging unavailable in design sample')}</span>`,
  );
}

const sampleAdvancedValuePreview = advancedValuePreview;
advancedValuePreview = function(info) {
  const target = state.cellValueTarget;
  if (target) {
    const identity = JSON.stringify([target.database, target.table, target.row, target.column]);
    if (state.valuePreviewTarget !== identity || !state.valuePreview) {
      state.valuePreviewTarget = identity;
      state.valuePreview = {
        phase: target.value === null ? 'null' : target.readonly ? 'readonly' : 'normal',
        mode: 'text',
        value: target.value == null ? '' : String(target.value),
        isNull: target.value === null,
        originalIsNull: target.value === null,
        nullType: 'text',
        chunk: 1,
        overwrite: false,
        path: '/tmp/datapier-edit-example.txt',
        validation: '',
        dirty: false,
        originalValue: target.value == null ? '' : String(target.value),
      };
    }
    if (target.readonly) state.valuePreview.phase = 'readonly';
  }
  let markup = sampleAdvancedValuePreview(info);
  if (target) {
    markup = markup.replace(
      'Local MySQL · commerce.orders',
      advancedEscape(`Local MySQL · ${target.database}.${target.table}`),
    )
      .replace(
        'id = 10422 · metadata',
        advancedEscape(
          `${typeof target.row === 'object' ? JSON.stringify(target.row) : target.row} · ${target.column}`,
        ),
      );
  }
  const phase = state.valuePreview.phase;
  const simulated = `<section class="detail-section"><h3>${
    advancedText('生命周期模拟', 'Lifecycle simulation')
  }</h3><p>${
    advancedText(
      '以下按钮只切换本地示例状态，不访问文件或启动进程。',
      'These buttons only change local sample states; no file or process is accessed.',
    )
  }</p>${
    advancedButton(
      'file-demo-prepare',
      advancedText('模拟准备文件', 'Simulate file preparation'),
      !!target?.readonly || !!state.valuePreview.cleanupTarget,
    )
  } ${
    advancedButton(
      'file-demo-launch',
      advancedText('模拟终端交接', 'Simulate terminal handoff'),
      !state.valuePreview.demoFile || phase === 'file-running' || !!state.valuePreview.cleanupTarget,
    )
  } ${
    advancedButton(
      'file-demo-read',
      advancedText('模拟读取修改', 'Simulate reading changes'),
      phase !== 'file-returned' || !!target?.readonly,
    )
  } ${
    advancedButton(
      'file-demo-keep',
      advancedText('模拟保留文件', 'Simulate keeping file'),
      !state.valuePreview.demoFile || !!state.valuePreview.cleanupTarget,
    )
  } ${
    advancedButton(
      'file-demo-cleanup',
      advancedText('模拟清理确认', 'Simulate cleanup confirmation'),
      !state.valuePreview.demoFile || !!state.valuePreview.cleanupTarget,
    )
  }${
    phase === 'file-running'
      ? `<section class="detail-section" aria-label="${advancedText('终端面板预览', 'Terminal pane preview')}"><h4>${
        advancedText('终端面板预览', 'Terminal pane preview')
      }</h4><pre>${advancedEscape(`$ editor ${state.valuePreview.demoFile.path}\n[simulated editor running]`)}</pre>${
        advancedButton('file-demo-return', advancedText('返回值编辑器', 'Return to value editor'))
      }</section>`
      : ''
  }${
    phase === 'file-returned'
      ? `<p>${
        advancedText(
          '已返回值编辑器；修改尚未读取或回填。',
          'Returned to the value editor; changes have not been read or applied.',
        )
      }</p>`
      : ''
  }${
    state.valuePreview.cleanupTarget
      ? `<section class="detail-section"><h4>${
        advancedText('确认清理示例文件', 'Confirm sample file cleanup')
      }</h4><p>${
        advancedText(
          '目标已冻结；清理后此会话无法恢复。不会关闭编辑器。',
          'Target frozen; cleanup cannot be recovered from this session. It will not close the editor.',
        )
      }</p><dl class="facts">${fact(advancedText('身份', 'Identity'), state.valuePreview.cleanupTarget.identity)}${
        fact(advancedText('版本', 'Version'), String(state.valuePreview.cleanupTarget.version))
      }${fact(advancedText('路径', 'Path'), state.valuePreview.cleanupTarget.path)}</dl>${
        advancedButton('file-demo-cleanup-cancel', advancedText('继续编辑', 'Continue editing'))
      } <button type="button" class="button danger" data-preview-action="file-demo-cleanup-confirm">${
        advancedText('确认清理示例', 'Confirm sample cleanup')
      }</button></section>`
      : ''
  }</section>`;
  return markup.replace('</div><footer class="modal-foot">', `${simulated}</div><footer class="modal-foot">`);
};

// 与共享关闭逻辑分离：这里只更新示例内状态，不触发真实动作。
function prepareReplacement(s) {
  if (!s.find || state.connectionModes[s.source] || !(s.source === 'mysql' ? s.columns.length : s.roles.length)) return;
  try {
    s.plan = replacementPlan(s);
    s.phase = 'frozen';
    s.page = 1;
    s.chunk = 1;
    s.error = '';
  } catch (error) {
    s.error = advancedText('正则表达式无效：', 'Invalid regular expression: ') + error.message;
  }
}

function initialKindForReplacement() {
  const key = [...state.selectedKeys][0] || keySamples.find(item => item.name === state.key);
  return { STR: 'string', HASH: 'hash', LIST: 'list', SET: 'set', ZSET: 'zset' }[key?.type] || 'string';
}

function updateAdvancedHex(target, text) {
  const start = target.phase === 'large' ? (target.chunk - 1) * 64 : 0;
  const count = Math.min(target.phase === 'large' ? 64 : target.bytes.length, target.bytes.length - start);
  const parts = text.trim() ? text.trim().split(/\s+/) : [];
  if (parts.length !== count || parts.some(part => !/^[0-9a-f]{2}$/i.test(part))) {
    target.hexError = true;
    target.hexDraft = text;
    target.validation = advancedText(
      '请为当前窗口输入相同数量的两位十六进制字节。',
      'Enter the same number of two-digit hexadecimal bytes for this window.',
    );
    return;
  }
  const bytes = target.bytes.slice();
  bytes.set(parts.map(part => parseInt(part, 16)), start);
  target.bytes = bytes;
  target.hexDraft = null;
  target.dirty = true;
  target.isNull = false;
  try {
    target.value = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    target.hexError = false;
    target.validation = '';
  } catch (_) {
    target.hexError = true;
    target.validation = advancedText(
      '当前文本目标要求有效 UTF-8；字节已保留，请修正后应用。',
      'This text target requires valid UTF-8; bytes are retained until corrected.',
    );
  }
}

document.addEventListener('change', event => {
  const el = event.target;
  if (!(el instanceof HTMLElement)) return;
  const field = el.dataset.previewField;
  const replacement = state.replacementPreview;
  const value = state.valuePreview;
  if (el.dataset.previewInclude !== undefined && replacement?.plan) {
    const item = replacement.plan.items[Number(el.dataset.previewInclude)];
    if (item && !item.collision && replacement.phase === 'frozen') item.included = el.checked;
  } else if (el.dataset.previewGroup && replacement) {
    const list = replacement[el.dataset.previewGroup];
    replacement[el.dataset.previewGroup] = el.checked
      ? [...new Set([...list, el.value])]
      : list.filter(item => item !== el.value);
  } else if (field === 'replace-phase' && replacement) {
    if (el.value === 'editing' || el.value === 'restored') {
      replacement.plan = null;
      replacement.phase = el.value;
      replacement.error = '';
    } else if (el.value === 'readonly') replacement.phase = el.value;
    else if (replacement.plan) {
      replacement.phase =
        el.value === 'frozen' && JSON.stringify(replacementContext(replacement)) !== replacement.plan.signature
          ? 'stale'
          : el.value;
    } else if (el.value === 'frozen') prepareReplacement(replacement);
    else replacement.phase = el.value;
  } else if (field === 'value-phase' && value) {
    if (el.value === 'large' && value.phase !== 'large') {
      value.bytes = new TextEncoder().encode('Sample large value: abcdef0123456789\n'.repeat(512));
      value.value = new TextDecoder().decode(value.bytes);
      value.dirty = false;
    }
    value.chunk = 1;
    value.hexDraft = null;
    value.hexError = false;
    value.validation = '';
    value.phase = el.value;
    if (el.value === 'null') value.isNull = true;
    if (el.value.startsWith('image-')) value.mode = 'image';
  } else if (field && el.closest('.advanced-preview')) {
    const isValue = el.closest('.advanced-preview').querySelector('[data-preview-field="value-phase"]');
    const target = isValue ? value : replacement;
    if (!target) return;
    if (isValue && (field === 'hexWindow' || field === 'value')) return;
    target[field] = el.type === 'checkbox' ? el.checked : el.value;
    if (!isValue && field === 'kind') target.roles = [el.value === 'set' || el.value === 'zset' ? 'member' : 'value'];
    if (!isValue && field === 'source' && el.value === 'redis') {
      target.kind = initialKindForReplacement();
      target.roles = [target.kind === 'set' || target.kind === 'zset' ? 'member' : 'value'];
    }
  } else return;
  render();
});
document.addEventListener('input', event => {
  const el = event.target;
  if (
    !(el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) || !el.dataset.previewField
    || !el.closest('.advanced-preview')
  ) return;
  const isValue = el.closest('.advanced-preview').querySelector('[data-preview-field="value-phase"]');
  const target = isValue ? state.valuePreview : state.replacementPreview;
  if (!target || el.type === 'checkbox') return;
  target[el.dataset.previewField] = el.value;
  if (isValue && el.dataset.previewField === 'hexWindow') {
    updateAdvancedHex(target, el.value);
  }
  if (isValue && el.dataset.previewField === 'value') {
    target.bytes = null;
    target.dirty = true;
    target.isNull = false;
    if (target.phase === 'null') target.phase = 'normal';
  }
  if (isValue && ['value', 'hexWindow'].includes(el.dataset.previewField)) {
    const button = el.closest('.advanced-preview').querySelector('[data-preview-action="value-apply"]');
    if (button) button.disabled = !target.dirty || !!target.hexError;
  }
});
document.addEventListener('click', event => {
  const button = event.target.closest('[data-preview-action], [data-preview-mode]');
  if (!button) return;
  const r = state.replacementPreview, v = state.valuePreview;
  const action = button.dataset.previewAction;
  if (button.dataset.previewMode && v) {
    if (v.hexDraft != null) return;
    v.mode = button.dataset.previewMode;
    v.chunk = 1;
  } else if (action === 'replace-edit') {
    r.phase = 'editing';
    r.plan = null;
    r.error = '';
    r.page = 1;
    r.chunk = 1;
  } else if (action === 'replace-prepare') prepareReplacement(r);
  else if (action === 'replace-prev') {
    r.page = Math.max(1, r.page - 1);
    r.chunk = 1;
  } else if (action === 'replace-next') {
    r.page = Math.min(Math.max(1, Math.ceil((r.plan?.items.length || 0) / Number(r.pageSize || 1))), r.page + 1);
    r.chunk = 1;
  } else if (action === 'replace-chunk') {
    r.chunk = r.chunk * 64 >= (r.plan?.items[(r.page - 1) * Number(r.pageSize || 1)]?.before.length || 0)
      ? 1
      : r.chunk + 1;
  } else if (action === 'replace-conflict') r.phase = 'conflict';
  else if (action === 'replace-rebaseline') prepareReplacement(r);
  else if (action === 'value-prev' && v.hexDraft == null) v.chunk = Math.max(1, v.chunk - 1);
  else if (action === 'value-next' && v.hexDraft == null) {
    v.chunk = Math.min(Math.ceil(v.bytes.length / 64), v.chunk + 1);
  } else if (action === 'value-image') v.phase = 'image-ready';
  else if (action === 'file-demo-prepare') {
    v.fileVersion = (v.fileVersion || 0) + 1;
    v.demoFile = { identity: state.valuePreviewTarget || 'sample-value', version: v.fileVersion, path: v.path };
    v.phase = 'file-ready';
  } else if (action === 'file-demo-launch' && v.demoFile) v.phase = 'file-running';
  else if (action === 'file-demo-return' && v.phase === 'file-running') v.phase = 'file-returned';
  else if (action === 'file-demo-read' && v.phase === 'file-returned') {
    v.phase = 'file-read';
    v.value += '\n' + advancedText('外部编辑示例', 'External edit sample');
    v.bytes = null;
    v.dirty = true;
  } else if (action === 'file-demo-keep') v.phase = 'file-kept';
  else if (action === 'file-demo-cleanup' && v.demoFile) {
    v.cleanupReturn = v.phase;
    v.cleanupTarget = { ...v.demoFile };
    v.phase = 'file-cleanup';
  } else if (action === 'file-demo-cleanup-cancel') {
    v.cleanupTarget = null;
    v.phase = v.cleanupReturn;
  } else if (action === 'file-demo-cleanup-confirm' && v.cleanupTarget) {
    v.demoFile = null;
    v.cleanupTarget = null;
    v.phase = 'normal';
    v.validation = advancedText(
      '示例文件已模拟清理；没有删除真实文件。',
      'Sample cleanup simulated; no real file was deleted.',
    );
  } else if (action === 'value-apply') {
    v.dirty = false;
    v.originalValue = v.value;
    v.originalIsNull = Boolean(v.isNull);
    if (state.cellValueTarget && !state.cellValueTarget.readonly) {
      state.cellInspectorValue = v.value;
      state.cellInspectorIsNull = Boolean(v.isNull);
      state.cellInspectorDirty = true;
      dialogDraftDirty = true;
      state.overlay = state.cellAdvancedReturn ?? null;
      state.cellAdvancedReturn = null;
      state.cellValueTarget = null;
    }
  } else if (action === 'value-null') {
    v.phase = 'null';
    v.isNull = true;
    v.dirty = true;
  } else if (action === 'value-continue') v.phase = 'normal';
  else if (action === 'value-discard') {
    v.isNull = Boolean(v.originalIsNull);
    v.phase = v.isNull ? 'null' : 'normal';
    v.dirty = false;
    v.value = v.originalValue ?? '{"status":"pending"}';
    v.bytes = null;
    v.hexDraft = null;
    v.hexError = false;
  } else if (action === 'value-validate' || action === 'value-format') {
    try {
      if (v.mode === 'json') {
        const formatted = formatAdvancedJson(v.value, action === 'value-format');
        if (action === 'value-format') {
          v.value = formatted;
          v.dirty = true;
        }
      } else {
        const parsed = new DOMParser().parseFromString(v.value, 'application/xml');
        if (parsed.querySelector('parsererror')) throw new Error('XML');
        if (action === 'value-format') {
          v.value = new XMLSerializer().serializeToString(parsed);
          v.dirty = true;
        }
      }
      if (action === 'value-format') v.bytes = null;
      v.validation = advancedText(
        action === 'value-format' ? '格式化完成；修改尚未应用。' : '校验通过。',
        action === 'value-format' ? 'Formatted; changes are unapplied.' : 'Validation succeeded.',
      );
    } catch (_) {
      v.validation = advancedText('格式无效；保留原值。', 'Invalid format; the original value is retained.');
    }
  } else return;
  render();
});
document.addEventListener('keydown', event => {
  const tab = event.target.closest('[data-preview-mode]');
  if (!tab || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  const tabs = [...tab.parentElement.querySelectorAll('[data-preview-mode]')];
  const index = tabs.indexOf(tab);
  const next = event.key === 'Home'
    ? 0
    : event.key === 'End'
    ? tabs.length - 1
    : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
  event.preventDefault();
  tabs[next].focus();
});

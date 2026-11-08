'use strict';

// 结构页只处理页面内示例，不连接服务器，也不生成可执行凭据。
const schemaText = (zh, en) => language === 'zh-CN' ? zh : en;
const schemaTypes = ['BIGINT', 'INT', 'DECIMAL(12,2)', 'VARCHAR(255)', 'TEXT', 'DATETIME', 'DATE', 'BOOLEAN'];

function schemaInitialColumns(tab) {
  const sample = databases[tab.database]?.[tab.table]?.[0] || {};
  return Object.keys(sample).map((name, index) => ({
    id: index,
    name,
    type: name === 'id' || name.endsWith('_id')
      ? 'BIGINT'
      : ['total', 'revenue', 'price'].includes(name)
      ? 'DECIMAL(12,2)'
      : name === 'created_at'
      ? 'DATETIME'
      : 'VARCHAR(255)',
    nullable: name === 'email',
    defaultKind: 'none',
    defaultValue: '',
    comment: sampleColumnComments[tab.database]?.[tab.table]?.[name] ?? '',
    primary: name === 'id',
    autoIncrement: name === 'id',
  }));
}

function schemaState(tab) {
  if (!tab.schemaDesign) {
    const original = schemaInitialColumns(tab);
    tab.schemaDesign = {
      original,
      columns: original.map(column => ({ ...column })),
      editing: false,
      plan: null,
    };
  }
  return tab.schemaDesign;
}

function schemaHasDraft(tab) {
  const design = tab?.schemaDesign;
  return !!design && design.columns.some((column, index) => {
    const original = design.original[index];
    return column.name !== original.name || column.type !== original.type
      || column.nullable !== original.nullable || column.defaultKind !== original.defaultKind
      || (column.defaultKind === 'literal' && column.defaultValue !== original.defaultValue)
      || column.comment !== original.comment;
  });
}

function schemaErrors(design) {
  const names = new Map();
  design.columns.forEach(column => {
    const name = column.name.toLocaleLowerCase();
    names.set(name, (names.get(name) || 0) + 1);
  });
  return design.columns.map((column, index) => {
    const errors = {};
    if (!column.name.trim()) errors.name = schemaText('字段名不能为空。', 'Column name is required.');
    else if (names.get(column.name.toLocaleLowerCase()) > 1) {
      errors.name = schemaText('字段名不能重复。', 'Column names must be unique.');
    } else if (
      design.original.some((other, i) =>
        i !== index && other.name.toLocaleLowerCase() === column.name.toLocaleLowerCase()
      )
    ) {
      errors.name = schemaText(
        '字段名与其他原始字段冲突；请分步重命名。',
        'This name belongs to another original column; rename in separate steps.',
      );
    }
    if (!column.nullable && column.defaultKind === 'null') {
      errors.defaultKind = schemaText('非空字段不能默认 NULL。', 'A NOT NULL column cannot default to NULL.');
    }
    if (column.autoIncrement && column.defaultKind !== 'none') {
      errors.defaultKind = schemaText('自增字段不能设置默认值。', 'An AUTO_INCREMENT column cannot have a default.');
    }
    if (column.autoIncrement && !['BIGINT', 'INT'].includes(column.type)) {
      errors.type = schemaText('自增字段必须使用整数类型。', 'An AUTO_INCREMENT column requires an integer type.');
    }
    return errors;
  });
}

function schemaQuoteIdentifier(value) {
  return '`' + value.replace(/`/g, '``') + '`';
}

function schemaQuoteText(value) {
  // 按 MySQL 常规反斜杠转义展示示例；真实 SQL mode 由 Engine 处理。
  return "'" + value.replace(/\\/g, '\\\\').replace(/'/g, "''")
    .replace(/\0/g, '\\0').replace(/\n/g, '\\n').replace(/\r/g, '\\r')
    .replace(/\x1a/g, '\\Z')
    + "'";
}

function schemaDDL(tab, design) {
  const clauses = [];
  design.columns.forEach((column, index) => {
    const original = design.original[index];
    const changed = column.name !== original.name || column.type !== original.type
      || column.nullable !== original.nullable || column.defaultKind !== original.defaultKind
      || (column.defaultKind === 'literal' && column.defaultValue !== original.defaultValue)
      || column.comment !== original.comment;
    if (!changed) return;
    let definition = `CHANGE COLUMN ${schemaQuoteIdentifier(original.name)} ${
      schemaQuoteIdentifier(column.name)
    } ${column.type}`;
    definition += column.nullable ? ' NULL' : ' NOT NULL';
    if (column.defaultKind === 'null') definition += ' DEFAULT NULL';
    else if (column.defaultKind === 'literal') definition += ` DEFAULT ${schemaQuoteText(column.defaultValue)}`;
    if (column.autoIncrement) definition += ' AUTO_INCREMENT';
    definition += ` COMMENT ${schemaQuoteText(column.comment)}`;
    clauses.push(definition);
  });
  return `ALTER TABLE ${schemaQuoteIdentifier(tab.database)}.${schemaQuoteIdentifier(tab.table)}\n  ${
    clauses.join(',\n  ')
  };`;
}

function schemaDesignAction(action) {
  if (!['schema-edit', 'schema-discard', 'schema-preview', 'schema-resume', 'schema-example'].includes(action)) {
    return false;
  }
  const tab = state.tabs.find(item => item.id === state.active && !item.query);
  if (!tab || (state.connectionModes.mysql && action !== 'schema-discard')) {
    render();
    return true;
  }
  const design = schemaState(tab);
  let message = null;
  if (action === 'schema-edit') {
    design.editing = true;
  } else if (action === 'schema-discard') {
    design.columns = design.original.map(column => ({ ...column }));
    design.editing = false;
    design.plan = null;
  } else if (action === 'schema-resume') {
    design.plan = null;
    design.editing = true;
  } else {
    if (action === 'schema-example' && !schemaHasDraft(tab)) {
      const column = design.columns.find(item => item.name === 'status') || design.columns.find(item => !item.primary);
      if (column) {
        column.comment = column.comment
          ? `${column.comment} · ${schemaText('示例', 'Example')}`
          : schemaText('示例注释', 'Example comment');
      }
    }
    design.editing = true;
    if (schemaHasDraft(tab) && !schemaErrors(design).some(errors => Object.keys(errors).length)) {
      design.plan = schemaDDL(tab, design);
    } else {
      design.plan = null;
      message = schemaHasDraft(tab)
        ? schemaText('请先修正字段错误。', 'Correct the column errors first.')
        : schemaText('请先修改字段。', 'Change a column first.');
    }
  }
  render();
  if (message) toast(message);
  return true;
}

function schemaStructurePanel(tab) {
  const design = schemaState(tab);
  const readOnly = state.connectionModes.mysql;
  const editable = design.editing && !design.plan && !readOnly;
  const errors = schemaErrors(design);
  const hasErrors = errors.some(item => Object.keys(item).length);
  const text = schemaText;
  const field = (column, key, input, error) =>
    `<div class="schema-field">${input}${
      error ? `<span class="schema-error" role="alert">${h(error)}</span>` : ''
    }</div>`;
  const attr = (column, key) =>
    `id="schema-${h(tab.id)}-${column.id}-${key}" data-schema-column="${column.id}" data-schema-field="${key}"`;
  const errorAttr = error => error ? ' aria-invalid="true"' : '';
  const columns = design.columns.map((column, index) => {
    const error = errors[index];
    const cell = (key, label, value) =>
      editable
        ? field(
          column,
          key,
          `<input type="text" ${attr(column, key)} aria-label="${h(label)}"${errorAttr(error[key])} value="${
            h(value)
          }">`,
          error[key],
        )
        : `<span class="schema-value">${h(value || '—')}</span>`;
    const name = cell('name', text('字段名', 'Column name'), column.name);
    const type = editable
      ? field(
        column,
        'type',
        `<select ${attr(column, 'type')} aria-label="${h(text('类型', 'Type'))}"${errorAttr(error.type)}>${
          schemaTypes.map(type =>
            `<option value="${h(type)}"${column.type === type ? ' selected' : ''}>${h(type)}</option>`
          ).join('')
        }</select>`,
        error.type,
      )
      : h(column.type);
    const nullable = editable
      ? `<select ${attr(column, 'nullable')} aria-label="${h(text('允许 NULL', 'Allow NULL'))}"${
        column.primary ? ' disabled' : ''
      }><option value="false"${!column.nullable ? ' selected' : ''}>${
        h(text('否', 'No'))
      }</option><option value="true"${column.nullable ? ' selected' : ''}>${h(text('是', 'Yes'))}</option></select>`
      : h(column.nullable ? text('是', 'Yes') : text('否', 'No'));
    const defaultLabel = column.defaultKind === 'none'
      ? text('无默认值', 'No default')
      : column.defaultKind === 'null'
      ? 'NULL'
      : column.defaultValue;
    const defaultValue = editable
      ? field(
        column,
        'defaultKind',
        `<select ${attr(column, 'defaultKind')} aria-label="${h(text('默认值类型', 'Default kind'))}"${
          errorAttr(error.defaultKind)
        }><option value="none"${column.defaultKind === 'none' ? ' selected' : ''}>${
          h(text('无默认值', 'No default'))
        }</option><option value="null"${
          column.defaultKind === 'null' ? ' selected' : ''
        }>NULL</option><option value="literal"${column.defaultKind === 'literal' ? ' selected' : ''}>${
          h(text('字面量', 'Literal'))
        }</option></select>${
          column.defaultKind === 'literal'
            ? `<input type="text" ${attr(column, 'defaultValue')} aria-label="${
              h(text('默认字面量', 'Default literal'))
            }" value="${h(column.defaultValue)}">`
            : ''
        }`,
        error.defaultKind,
      )
      : `<span class="schema-value">${h(defaultLabel)}</span>`;
    const comment = editable
      ? `<textarea ${attr(column, 'comment')} rows="2" aria-label="${h(text('注释', 'Comment'))}">${
        h(column.comment)
      }</textarea>`
      : `<span class="schema-value">${h(column.comment || '—')}</span>`;
    return `<tr><td>${name}</td><td>${type}</td><td>${nullable}</td><td>${defaultValue}</td><td>${
      column.primary ? 'PRIMARY KEY · AUTO_INCREMENT' : '—'
    }</td><td>${comment}</td></tr>`;
  }).join('');
  const toolbar = readOnly
    ? `${
      schemaHasDraft(tab)
        ? `<button data-action="schema-discard">${h(text('丢弃修改', 'Discard changes'))}</button>`
        : ''
    }<span class="schema-hint">${
      h(text('只读连接：不可编辑或预览。', 'Read-only connection: editing and preview are unavailable.'))
    }</span>`
    : design.plan
    ? `<button data-action="schema-resume">${
      h(text('丢弃计划 / 继续编辑', 'Discard plan / continue editing'))
    }</button><button data-action="schema-discard">${h(text('丢弃修改', 'Discard changes'))}</button>`
    : editable
    ? `<button data-action="schema-discard">${
      h(text('丢弃修改', 'Discard changes'))
    }</button><button class="primary" data-action="schema-preview"${
      !schemaHasDraft(tab) || hasErrors ? ' disabled' : ''
    }>${h(text('预览 DDL', 'Preview DDL'))}</button>`
    : `<button data-action="schema-edit">${h(text('编辑结构', 'Edit structure'))}</button>`;
  return `<div id="schema-structure-panel" class="schema-designer" data-schema-tab="${
    h(tab.id)
  }"><div class="schema-toolbar">${toolbar}<span class="schema-toolbar-spacer"></span><button disabled title="${
    h(text('设计预览不连接数据库，也不执行 SQL。', 'The design preview does not connect to a database or execute SQL.'))
  }">${h(text('提交', 'Submit'))}</button><span class="schema-hint">${
    h(text('提交在设计预览中不可用', 'Submit is unavailable in this design preview'))
  }</span></div><div class="schema-designer-scroll"><p class="schema-notice">${
    h(text(
      '示例结构仅由本地样例推断，不连接数据库，也不执行 SQL。索引、约束和表选项不是服务器读取结果。DDL 示例使用 MySQL 常规反斜杠转义。',
      'Sample structure is inferred locally; no database is connected and no SQL is executed. Indexes, constraints, and table options are not read from a server. The DDL example uses standard MySQL backslash escaping.',
    ))
  }</p>${
    design.plan && !readOnly
      ? `<section class="schema-section"><h3>${
        h(text('冻结的 DDL 预览', 'Frozen DDL preview'))
      }</h3><p class="schema-hint">${
        h(text(
          '定义已冻结；继续编辑将丢弃此计划并保留修改。',
          'Definitions are frozen; continuing discards this plan and keeps your changes.',
        ))
      }</p><pre class="schema-ddl">${h(design.plan)}</pre></section>`
      : ''
  }<section class="schema-section"><h3>${
    h(text('字段', 'Columns'))
  }</h3><div class="schema-table-scroll"><table class="schema-table"><thead><tr><th>${
    h(text('字段', 'Column'))
  }</th><th>${h(text('类型', 'Type'))}</th><th>NULL</th><th>${h(text('默认值', 'Default'))}</th><th>${
    h(text('约束', 'Constraint'))
  }</th><th>${
    h(text('注释', 'Comment'))
  }</th></tr></thead><tbody>${columns}</tbody></table></div></section><section class="schema-section"><h3>${
    h(text('索引与约束', 'Indexes and constraints'))
  }</h3><p>${h(text('示例主键：', 'Sample primary key: '))}${
    h(design.columns.find(column => column.primary)?.name || '—')
  } · PRIMARY KEY · AUTO_INCREMENT</p><p class="schema-hint">${
    h(text('其他索引与约束未提供。', 'Other indexes and constraints are not provided.'))
  }</p></section><section class="schema-section"><h3>${h(text('表选项', 'Table options'))}</h3><p class="schema-hint">${
    h(text(
      '示例数据未提供引擎、字符集或其他表选项。',
      'The sample data does not provide an engine, character set, or other table options.',
    ))
  }</p></section></div></div>`;
}

function schemaInput(event) {
  const target = event.target;
  if (!(target instanceof HTMLElement) || !target.dataset.schemaField) return;
  const panel = target.closest('#schema-structure-panel');
  if (!panel || !panel.isConnected) return;
  const tab = state.tabs.find(item => String(item.id) === panel.dataset.schemaTab);
  if (!tab || tab.id !== state.active || state.connectionModes.mysql) return;
  const design = tab.schemaDesign;
  if (!design?.editing || design.plan) return;
  const column = design.columns.find(item => String(item.id) === target.dataset.schemaColumn);
  const key = target.dataset.schemaField;
  if (!column || !['name', 'type', 'nullable', 'defaultKind', 'defaultValue', 'comment'].includes(key)) return;
  column[key] = key === 'nullable' ? target.value === 'true' : target.value;
  if (event.isComposing) return;
  render();
}

document.addEventListener('input', event => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) schemaInput(event);
});
document.addEventListener('change', event => {
  if (event.target instanceof HTMLSelectElement) schemaInput(event);
});
document.addEventListener('compositionend', event => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) schemaInput(event);
});

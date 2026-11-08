'use strict';

// 结构页只处理页面内示例，不连接服务器，也不生成可执行凭据。
const schemaText = (zh, en) => language === 'zh-CN' ? zh : en;
const schemaTypes = [
  'BIGINT',
  'INT',
  'DECIMAL(12,2)',
  'NUMERIC(12,2)',
  'VARCHAR(255)',
  'CHAR(1)',
  'VARBINARY(255)',
  'BINARY(1)',
  'BIT(1)',
  'TEXT',
  'DATETIME',
  'TIMESTAMP',
  'TIME',
  'DATE',
  'BOOLEAN',
];

function schemaTypeParts(type) {
  const match = /^(\s*([a-zA-Z]+)\s*)(?:\((.*)\))?(.*)$/s.exec(type);
  if (!match) return null;
  const family = match[2].toUpperCase();
  const decimal = ['DECIMAL', 'NUMERIC'].includes(family);
  const fractionalSeconds = ['DATETIME', 'TIMESTAMP', 'TIME'].includes(family);
  const length = fractionalSeconds || ['CHAR', 'VARCHAR', 'BINARY', 'VARBINARY', 'BIT'].includes(family);
  if (!decimal && !length) return { family, suffix: match[4] };
  const params = match[3] === undefined
    ? (decimal ? ['10', '0'] : [fractionalSeconds ? '0' : '1'])
    : match[3].split(',');
  return { family, decimal, length, fractionalSeconds, prefix: match[1], suffix: match[4], params };
}

function schemaTypeParameterErrors(column) {
  const parts = schemaTypeParts(column.type);
  if (!parts?.decimal && !parts?.length) return {};
  const integer = value => /^\d+$/.test(value ?? '') ? Number(value) : NaN;
  const first = integer(parts.params[0]?.trim());
  if (parts.decimal) {
    const second = parts.params.length === 1 ? 0 : integer(parts.params[1]?.trim());
    if (!Number.isInteger(first) || first < 1 || first > 65) {
      return { typeParameter: schemaText('精度必须为 1 到 65 的整数。', 'Precision must be an integer from 1 to 65.') };
    }
    if (
      !Number.isInteger(second) || second < 0 || second > 30 || second > first || parts.params.length > 2
    ) {
      return {
        typeParameter: schemaText(
          '小数位必须为 0 到 30 的整数，且不能大于精度。',
          'Scale must be an integer from 0 to 30 and no greater than precision.',
        ),
      };
    }
    return {};
  }
  const min = parts.family === 'BIT' ? 1 : 0;
  const max = parts.fractionalSeconds
    ? 6
    : parts.family === 'BIT'
    ? 64
    : ['CHAR', 'BINARY'].includes(parts.family)
    ? 255
    : 65535;
  return !Number.isInteger(first) || first < min || first > max || parts.params.length !== 1
    ? {
      typeParameter: schemaText(
        `长度必须为 ${min} 到 ${max} 的整数。`,
        `Length must be an integer from ${min} to ${max}.`,
      ),
    }
    : {};
}

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
    generated: '',
    stored: false,
    visible: true,
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
  const design = tab.schemaDesign;
  if (!design.collections) {
    const primaryColumns = design.original.filter(column => column.primary);
    const indexes = primaryColumns.length
      ? [{
        id: -1,
        name: '',
        kind: 'PRIMARY',
        visible: true,
        method: '',
        options: '',
        parts: primaryColumns.map((column, index) => ({
          id: -index - 1,
          column: column.name,
          expression: '',
          prefix: '',
          direction: 'ASC',
        })),
      }]
      : [];
    design.collections = { indexes, foreignKeys: [], checks: [] };
    design.originalCollections = JSON.parse(JSON.stringify(design.collections));
    for (const column of [...design.original, ...design.columns]) delete column.primary;
  }
  design.originalCollections ||= { indexes: [], foreignKeys: [], checks: [] };
  tab.schemaDesign.collectionSelection ||= {};
  tab.schemaDesign.section ||= 'columns';
  return tab.schemaDesign;
}

function schemaHasDraft(tab) {
  const design = tab?.schemaDesign && schemaState(tab);
  return !!design
    && (JSON.stringify(design.columns) !== JSON.stringify(design.original) || !!tab.schemaExamplesDirty
      || JSON.stringify(design.collections.indexes.filter(index => index.kind !== 'PRIMARY'))
        !== JSON.stringify(design.originalCollections.indexes.filter(index => index.kind !== 'PRIMARY'))
      || JSON.stringify(schemaPrimaryDefinition(schemaPrimary(design)))
        !== JSON.stringify(schemaPrimaryDefinition(schemaPrimary({ collections: design.originalCollections })))
      || JSON.stringify(design.collections.foreignKeys) !== JSON.stringify(design.originalCollections.foreignKeys)
      || JSON.stringify(design.collections.checks) !== JSON.stringify(design.originalCollections.checks));
}

function schemaPrimary(design) {
  return design.collections.indexes.find(index => index.kind === 'PRIMARY');
}

function schemaPrimaryDefinition(index) {
  if (!index) return null;
  return {
    parts: index.parts.map(({ column, expression, prefix, direction }) => ({ column, expression, prefix, direction })),
    method: index.method,
    visible: index.visible,
    options: index.options,
  };
}

function schemaPrimaryColumn(design, name) {
  return !!name && !!schemaPrimary(design)?.parts.some(part => !part.expression && part.column === name);
}

function schemaRequirePrimaryNotNull(design) {
  const primary = schemaPrimary(design);
  if (!primary) return;
  for (const part of primary.parts) {
    const column = design.columns.find(item => item.name === part.column);
    if (column && !part.expression) {
      column.nullable = false;
      if (column.defaultKind === 'null') column.defaultKind = 'none';
    }
  }
}

function schemaRowButton(symbol, label, attributes, disabled = false, pressed = '') {
  return `<button type="button" class="icon-button${symbol === 'trash' ? ' danger' : ''}" title="${
    h(label)
  }" aria-label="${h(label)}" ${attributes}${disabled ? ' disabled' : ''}${pressed}>${icon(symbol)}</button>`;
}

function schemaErrors(design) {
  const names = new Map();
  design.columns.forEach(column => {
    const name = column.name.toLocaleLowerCase();
    names.set(name, (names.get(name) || 0) + 1);
  });
  return design.columns.map((column, index) => {
    const errors = schemaTypeParameterErrors(column);
    if (!column.name.trim()) errors.name = schemaText('字段名不能为空。', 'Column name is required.');
    else if (names.get(column.name.toLocaleLowerCase()) > 1) {
      errors.name = schemaText('字段名不能重复。', 'Column names must be unique.');
    } else if (
      design.original.some(other =>
        other.id !== column.id && other.name.toLocaleLowerCase() === column.name.toLocaleLowerCase()
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
    if (column.defaultKind === 'expression' && !schemaValidExpression(column.defaultValue)) {
      errors.defaultKind = schemaText(
        '默认表达式不能为空或包含语句边界、注释。',
        'The default expression cannot be empty or contain statement boundaries or comments.',
      );
    }
    if (column.generated && column.defaultKind !== 'none') {
      errors.defaultKind = schemaText('生成列不能设置默认值。', 'A generated column cannot have a default.');
    }
    if (
      column.autoIncrement && !['TINYINT', 'SMALLINT', 'MEDIUMINT', 'INT', 'INTEGER', 'BIGINT'].includes(
        schemaTypeParts(column.type)?.family,
      )
    ) {
      errors.type = schemaText('自增字段必须使用整数类型。', 'An AUTO_INCREMENT column requires an integer type.');
    }
    return errors;
  });
}

function schemaQuoteIdentifier(value) {
  return '`' + value.replace(/`/g, '``') + '`';
}

function schemaValidExpression(value) {
  return !!value.trim() && ![';', '--', '/*', '#'].some(marker => value.includes(marker));
}

function schemaQuoteText(value) {
  // 按 MySQL 常规反斜杠转义展示示例；真实 SQL mode 由 Engine 处理。
  return "'" + value.replace(/\\/g, '\\\\').replace(/'/g, "''")
    .replace(/\0/g, '\\0').replace(/\n/g, '\\n').replace(/\r/g, '\\r')
    .replace(/\x1a/g, '\\Z')
    + "'";
}

function schemaExampleClauses(s, baseline, includeAll = false) {
  const changed = keys => includeAll || keys.some(key => s[key] !== baseline?.[key]);
  const clauses = [];
  // 索引与约束由独立集合生成，表选项只在明确修改后加入计划。
  if (!includeAll && changed(['engine', 'charset', 'collation', 'options'])) {
    clauses.push(
      `ENGINE=${s.engine}, DEFAULT CHARACTER SET ${s.charset}, COLLATE ${s.collation}${
        s.options ? `, ${s.options}` : ''
      }`,
    );
  }
  return clauses;
}
function schemaCollectionClauses(collections) {
  const clauses = [];
  for (const index of collections.indexes) {
    const parts = index.parts.map(part =>
      `${
        part.expression
          ? `(${part.expression})`
          : `${schemaQuoteIdentifier(part.column)}${part.prefix ? `(${part.prefix})` : ''}`
      }${part.direction === 'DESC' ? ' DESC' : ''}`
    );
    clauses.push(
      `ADD ${index.kind === 'PRIMARY' ? 'PRIMARY KEY' : `${index.kind} ${schemaQuoteIdentifier(index.name)}`} (${
        parts.join(', ')
      })${index.method ? ` USING ${index.method}` : ''}${index.visible === false ? ' INVISIBLE' : ''}${
        index.options ? ` ${index.options}` : ''
      }`,
    );
  }
  for (const fk of collections.foreignKeys) {
    clauses.push(
      `ADD CONSTRAINT ${schemaQuoteIdentifier(fk.name)} FOREIGN KEY (${
        fk.mappings.map(mapping => schemaQuoteIdentifier(mapping.column)).join(', ')
      }) REFERENCES ${fk.refDb ? `${schemaQuoteIdentifier(fk.refDb)}.` : ''}${schemaQuoteIdentifier(fk.refTable)} (${
        fk.mappings.map(mapping => schemaQuoteIdentifier(mapping.refColumn)).join(', ')
      }) ON DELETE ${fk.onDelete} ON UPDATE ${fk.onUpdate}`,
    );
  }
  for (const check of collections.checks) {
    clauses.push(
      `ADD CONSTRAINT ${schemaQuoteIdentifier(check.name)} CHECK (${check.expression})${
        check.enabled ? ' ENFORCED' : ' NOT ENFORCED'
      }`,
    );
  }
  return clauses;
}

function schemaCollectionError(design) {
  const names = new Set(design.columns.map(column => column.name));
  if (design.collections.indexes.filter(index => index.kind === 'PRIMARY').length > 1) {
    return schemaText('只能有一个主键。', 'Only one primary key is allowed.');
  }
  for (const index of design.collections.indexes) {
    if (
      index.kind === 'PRIMARY' && (index.parts.some(part => part.expression || part.prefix)
        || new Set(index.parts.map(part => part.column)).size !== index.parts.length)
    ) {
      return schemaText('主键必须使用不重复的完整字段。', 'Primary key parts must be distinct full columns.');
    }
    if (
      (index.kind !== 'PRIMARY' && !index.name.trim()) || !index.parts.length
      || index.parts.some(part =>
        part.expression.trim()
          ? !!part.column || !!part.prefix
          : !names.has(part.column)
      )
    ) {
      return schemaText(
        '请填写索引名称；每项选择字段或表达式，表达式不能同时指定字段或前缀。',
        'Enter an index name. Each part needs a column or an expression; expressions cannot have a column or prefix.',
      );
    }
    if (index.parts.some(part => part.expression.trim() && !schemaValidExpression(part.expression))) {
      return schemaText(
        '索引表达式不能包含语句边界或注释。',
        'Index expressions cannot contain statement boundaries or comments.',
      );
    }
    if (index.parts.some(part => part.prefix && !/^[1-9]\d*$/.test(part.prefix))) {
      return schemaText('索引前缀长度必须为正整数。', 'The index prefix length must be a positive integer.');
    }
  }
  for (const fk of design.collections.foreignKeys) {
    if (
      !fk.name.trim() || !fk.refTable.trim() || !fk.mappings.length
      || fk.mappings.some(mapping => !names.has(mapping.column) || !mapping.refColumn.trim())
    ) {
      return schemaText(
        '请填写外键名称、引用表和每组本地／引用列。',
        'Enter the foreign key name, referenced table and every local/referenced column mapping.',
      );
    }
  }
  for (const check of design.collections.checks) {
    if (!check.name.trim() || !check.expression.trim()) {
      return schemaText('请填写 CHECK 名称和表达式。', 'Enter the CHECK name and expression.');
    }
  }
  return '';
}

function schemaExampleError(tab) {
  const s = tab.schemaExamples;
  if (!s) return '';
  if (s.object === 'new-table' && !s.tableName?.trim()) {
    return schemaText('请填写新表名。', 'Enter the new table name.');
  }
  if (s.object === 'trigger' && !tab.table) {
    return schemaText('请选择触发器目标表。', 'Select a target table for the trigger.');
  }
  if (s.object === 'trigger-list') {
    return schemaText('触发器列表不能生成结构计划。', 'A trigger list cannot generate a schema plan.');
  }
  if (s.object === 'view' && (!s.viewName.trim() || !s.select.trim())) {
    return schemaText('请填写视图名称和 SELECT。', 'Enter the view name and SELECT.');
  }
  if (
    s.object === 'trigger' && (!s.triggerName.trim() || !s.body.trim() || s.order !== 'NONE' && !s.orderTarget.trim())
  ) {
    return schemaText(
      '请填写触发器名称、正文及指定顺序的相对触发器。',
      'Enter the trigger name, body and relative trigger for the selected order.',
    );
  }
  return '';
}

function schemaDDL(tab, design) {
  const clauses = [];
  const examples = tab.schemaExamples;
  if (examples?.object === 'view') {
    return `CREATE ALGORITHM=${examples.algorithm} SQL SECURITY ${examples.security} VIEW ${
      schemaQuoteIdentifier(tab.database)
    }.${schemaQuoteIdentifier(examples.viewName)} AS\n${examples.select};`;
  }
  if (examples?.object === 'trigger') {
    return `CREATE TRIGGER ${schemaQuoteIdentifier(tab.database)}.${
      schemaQuoteIdentifier(examples.triggerName)
    } ${examples.timing} ${examples.event} ON ${schemaQuoteIdentifier(tab.database)}.${
      schemaQuoteIdentifier(tab.table)
    } FOR EACH ROW${
      examples.order === 'NONE' ? '' : ` ${examples.order} ${schemaQuoteIdentifier(examples.orderTarget)}`
    }\n${examples.body}`;
  }
  design.original.filter(original => !design.columns.some(column => column.id === original.id))
    .forEach(original => clauses.push(`DROP COLUMN ${schemaQuoteIdentifier(original.name)}`));
  design.columns.forEach((column, index) => {
    const original = design.original.find(item => item.id === column.id);
    const positionChanged = original && design.original.findIndex(item => item.id === column.id) !== index;
    if (original && JSON.stringify(column) === JSON.stringify(original) && !positionChanged) return;
    let definition = original
      ? `CHANGE COLUMN ${schemaQuoteIdentifier(original.name)} ${schemaQuoteIdentifier(column.name)} ${column.type}`
      : `ADD COLUMN ${schemaQuoteIdentifier(column.name)} ${column.type}`;
    if (column.charset) definition += ` CHARACTER SET ${schemaQuoteIdentifier(column.charset)}`;
    if (column.collation) definition += ` COLLATE ${schemaQuoteIdentifier(column.collation)}`;
    definition += column.nullable ? ' NULL' : ' NOT NULL';
    if (column.defaultKind === 'null') definition += ' DEFAULT NULL';
    else if (column.defaultKind === 'literal') definition += ` DEFAULT ${schemaQuoteText(column.defaultValue)}`;
    else if (column.defaultKind === 'expression') definition += ` DEFAULT ${column.defaultValue.trim()}`;
    if (column.generated) {
      definition += ` GENERATED ALWAYS AS (${column.generated}) ${column.stored ? 'STORED' : 'VIRTUAL'}`;
    }
    if (column.autoIncrement) definition += ' AUTO_INCREMENT';
    if (column.visible === false) definition += ' INVISIBLE';
    definition += ` COMMENT ${schemaQuoteText(column.comment)}`;
    definition += index === 0 ? ' FIRST' : ` AFTER ${schemaQuoteIdentifier(design.columns[index - 1].name)}`;
    clauses.push(definition);
  });
  if (examples && tab.schemaExamplesDirty) clauses.push(...schemaExampleClauses(examples, tab.schemaExamplesOriginal));
  const originalPrimary = design.originalCollections.indexes.find(index => index.kind === 'PRIMARY');
  const currentPrimary = schemaPrimary(design);
  const renamedPrimary = originalPrimary && {
    ...originalPrimary,
    parts: originalPrimary.parts.map(part => {
      const original = design.original.find(column => column.name === part.column);
      return { ...part, column: design.columns.find(column => column.id === original?.id)?.name || part.column };
    }),
  };
  const primaryChanged =
    JSON.stringify(schemaPrimaryDefinition(currentPrimary)) !== JSON.stringify(schemaPrimaryDefinition(renamedPrimary));
  if (originalPrimary && primaryChanged) clauses.unshift('DROP PRIMARY KEY');
  clauses.push(...schemaCollectionClauses({
    ...design.collections,
    indexes: design.collections.indexes.filter(index => index.kind !== 'PRIMARY' || primaryChanged),
  }));
  if (examples?.object === 'new-table') {
    const definitions = design.columns.map(column => {
      return `${schemaQuoteIdentifier(column.name)} ${column.type}${
        column.charset ? ` CHARACTER SET ${schemaQuoteIdentifier(column.charset)}` : ''
      }${column.collation ? ` COLLATE ${schemaQuoteIdentifier(column.collation)}` : ''}${
        column.nullable ? ' NULL' : ' NOT NULL'
      }${
        column.defaultKind === 'null'
          ? ' DEFAULT NULL'
          : column.defaultKind === 'literal'
          ? ` DEFAULT ${schemaQuoteText(column.defaultValue)}`
          : column.defaultKind === 'expression'
          ? ` DEFAULT ${column.defaultValue.trim()}`
          : ''
      }${column.generated ? ` GENERATED ALWAYS AS (${column.generated}) ${column.stored ? 'STORED' : 'VIRTUAL'}` : ''}${
        column.autoIncrement ? ' AUTO_INCREMENT' : ''
      }${column.visible === false ? ' INVISIBLE' : ''} COMMENT ${schemaQuoteText(column.comment)}`;
    });
    definitions.push(...schemaCollectionClauses(design.collections).map(clause => clause.slice(4)));
    return `CREATE TABLE ${schemaQuoteIdentifier(tab.database)}.${
      schemaQuoteIdentifier(examples.tableName || 'new_table')
    } (\n  ${
      definitions.join(',\n  ')
    }\n) ENGINE=${examples.engine} DEFAULT CHARACTER SET ${examples.charset} COLLATE ${examples.collation}${
      examples.options ? ` ${examples.options}` : ''
    };`;
  }
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
  let focusPreview = false;
  if (action === 'schema-edit') {
    design.editing = true;
  } else if (action === 'schema-discard') {
    design.columns = design.original.map(column => ({ ...column }));
    design.collections = JSON.parse(JSON.stringify(design.originalCollections));
    design.collectionSelection = {};
    design.renameReferences = {};
    if (tab.schemaExamplesOriginal) tab.schemaExamples = { ...tab.schemaExamplesOriginal };
    tab.schemaExamplesDirty = false;
    design.editing = false;
    design.plan = null;
  } else if (action === 'schema-resume') {
    design.plan = null;
    design.editing = true;
  } else {
    if (action === 'schema-example' && !schemaHasDraft(tab)) {
      const column = design.columns.find(item => item.name === 'status')
        || design.columns.find(item => !schemaPrimaryColumn(design, item.name));
      if (column) {
        column.comment = column.comment
          ? `${column.comment} · ${schemaText('示例', 'Example')}`
          : schemaText('示例注释', 'Example comment');
      }
    }
    design.editing = true;
    if (schemaHasDraft(tab) && !schemaErrors(design).some(errors => Object.keys(errors).length)) {
      if (
        tab.schemaExamples && ['incomplete', 'unknown', 'restore', 'result-unknown'].includes(tab.schemaExamples.status)
      ) {
        message = schemaText(
          '请先读取并确认完整基线，或核对未知结果。',
          'Read and confirm a complete baseline or resolve the unknown result first.',
        );
      } else if (schemaExampleError(tab) || schemaCollectionError(design)) {
        message = schemaExampleError(tab) || schemaCollectionError(design);
      } else {
        design.plan = schemaDDL(tab, design);
        design.planPage = 1;
        if (!tab.schemaExamples || ['table', 'new-table'].includes(tab.schemaExamples.object)) {
          design.section = 'ddl_preview';
          focusPreview = action === 'schema-preview';
        }
      }
    } else {
      design.plan = null;
      message = schemaHasDraft(tab)
        ? schemaText('请先修正字段错误。', 'Correct the column errors first.')
        : schemaText('请先修改字段。', 'Change a column first.');
    }
  }
  render();
  if (focusPreview) document.querySelector('#schema-structure-panel [data-schema-section="ddl_preview"]')?.focus();
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
  const sections = [
    ['columns', text('字段', 'Columns')],
    ['indexes', text('索引', 'Indexes')],
    ['foreignKeys', text('外键', 'Foreign keys')],
    ['checks', 'CHECK'],
    ['options', text('表选项', 'Table options')],
    ['ddl_preview', text('预览 DDL', 'Preview DDL')],
  ];
  const showSections = !tab.schemaExamples || ['table', 'new-table'].includes(tab.schemaExamples.object);
  const tabs = showSections
    ? `<div class="schema-tabs" role="tablist" aria-label="${h(text('结构分区', 'Structure sections'))}">${
      sections.map(([id, label]) =>
        `<button type="button" role="tab" data-schema-section="${id}" aria-selected="${
          design.section === id
        }" tabindex="${design.section === id ? 0 : -1}">${h(label)}</button>`
      ).join('')
    }</div>`
    : '';
  const columns = design.columns.map((column, index) => {
    const selected = (design.selectedColumn ?? design.columns[0]?.id) === column.id;
    const error = errors[index];
    const primary = schemaPrimaryColumn(design, column.name);
    const foreign = !!column.name
      && design.collections.foreignKeys.some(fk => fk.mappings.some(mapping => mapping.column === column.name));
    const attr = (key, label) =>
      `data-schema-column="${column.id}" data-schema-field="${key}" aria-label="${h(label)}"${
        error[key] ? ' aria-invalid="true"' : ''
      }`;
    const cell = (key, label, value) =>
      editable
        ? `<input ${attr(key, label)} value="${h(value)}">${
          error[key] ? `<span class="schema-error" data-schema-error="${key}">${h(error[key])}</span>` : ''
        }`
        : h(value || '—');
    const name = editable
      ? cell('name', text('字段名', 'Column name'), column.name)
      : `<button type="button" class="schema-column-select" data-schema-op="select" data-column-id="${column.id}" aria-pressed="${selected}">${
        h(column.name || text('未命名字段', 'Unnamed column'))
      }</button>`;
    const parts = schemaTypeParts(column.type);
    const families = schemaTypes.map(type => schemaTypeParts(type).family);
    const hasParameter = parts?.length || parts?.decimal;
    const parameterValue = hasParameter ? parts.params.join(',') : '';
    const type = editable
      ? `<select ${attr('type', text('类型', 'Type'))}>${
        !families.includes(parts?.family) ? `<option value="${h(column.type)}" selected>${h(column.type)}</option>` : ''
      }${
        schemaTypes.map(type =>
          `<option value="${h(type)}"${schemaTypeParts(type).family === parts?.family ? ' selected' : ''}>${
            h(
              schemaTypeParts(type).family === parts?.family
                ? hasParameter ? parts.family + parts.suffix : column.type
                : schemaTypeParts(type).family,
            )
          }</option>`
        ).join('')
      }</select>${error.type ? `<span class="schema-error" data-schema-error="type">${h(error.type)}</span>` : ''}`
      : h(hasParameter ? parts.family + parts.suffix : column.type);
    const parameter = hasParameter
      ? editable
        ? cell(
          'typeParameter',
          parts.fractionalSeconds
            ? text('小数秒精度（0–6）', 'Fractional seconds precision (0–6)')
            : text('长度', 'Length'),
          parameterValue,
        )
        : h(parameterValue)
      : '—';
    const nullable = `<input type="checkbox" ${attr('nullable', text('允许 NULL', 'Allow NULL'))}${
      column.nullable ? ' checked' : ''
    }${!editable || primary ? ' disabled' : ''}>`;
    const defaultLabel = column.defaultKind === 'none'
      ? text('无', 'None')
      : column.defaultKind === 'null'
      ? 'NULL'
      : column.defaultValue;
    const defaultValue = editable
      ? `<select ${attr('defaultKind', text('默认值类型', 'Default kind'))}><option value="none"${
        column.defaultKind === 'none' ? ' selected' : ''
      }>${text('无', 'None')}</option><option value="null"${
        column.defaultKind === 'null' ? ' selected' : ''
      }>NULL</option><option value="literal"${column.defaultKind === 'literal' ? ' selected' : ''}>${
        text('字面量', 'Literal')
      }</option><option value="expression"${column.defaultKind === 'expression' ? ' selected' : ''}>${
        text('表达式', 'Expression')
      }</option></select>${
        column.defaultKind === 'literal'
          ? cell('defaultValue', text('默认字面量', 'Default literal'), column.defaultValue)
          : column.defaultKind === 'expression'
          ? `<textarea ${attr('defaultValue', text('默认表达式', 'Default expression'))} rows="2">${
            h(column.defaultValue)
          }</textarea>`
          : ''
      }${
        error.defaultKind
          ? `<span class="schema-error" data-schema-error="defaultKind">${h(error.defaultKind)}</span>`
          : ''
      }`
      : h(defaultLabel);
    const keyLabel = primary ? text('取消主键', 'Remove from primary key') : text('设为主键', 'Add to primary key');
    const actions = [
      schemaRowButton(
        'key',
        keyLabel,
        `data-schema-op="primary" data-column-id="${column.id}"`,
        !editable || !column.name.trim(),
        ` aria-pressed="${primary}"`,
      ),
      schemaRowButton(
        'chev-up',
        text('上移字段', 'Move column up'),
        `data-schema-op="up" data-column-id="${column.id}"`,
        !editable || index === 0,
      ),
      schemaRowButton(
        'chev',
        text('下移字段', 'Move column down'),
        `data-schema-op="down" data-column-id="${column.id}"`,
        !editable || index === design.columns.length - 1,
      ),
      schemaRowButton(
        'trash',
        text('删除字段', 'Delete column'),
        `data-schema-op="delete" data-column-id="${column.id}"`,
        !editable,
      ),
    ].join('');
    return `<tr data-schema-row="${column.id}"${
      selected ? ' class="schema-row-selected"' : ''
    }><td>${name}</td><td>${type}</td><td>${parameter}</td><td>${nullable}</td><td>${defaultValue}</td><td class="schema-key-icons">${
      primary
        ? `<span role="img" title="${h(text('主键', 'Primary key'))}" aria-label="${h(text('主键', 'Primary key'))}">${
          icon('key')
        }</span>`
        : ''
    }${
      foreign
        ? `<span role="img" title="${h(text('外键', 'Foreign key'))}" aria-label="${h(text('外键', 'Foreign key'))}">${
          icon('link')
        }</span>`
        : ''
    }${!primary && !foreign ? '—' : ''}</td><td>${
      cell('comment', text('注释', 'Comment'), column.comment)
    }</td><td class="schema-row-actions">${actions}</td></tr>`;
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
    }>${h(text('生成计划', 'Generate plan'))}</button>`
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
  }</p>${tabs}${
    design.plan && !readOnly && (!showSections || design.section === 'ddl_preview')
      ? `<section class="schema-section"><h3>${
        h(text('冻结的 DDL 预览', 'Frozen DDL preview'))
      }</h3><p class="schema-hint">${
        h(text(
          '定义已冻结；继续编辑将丢弃此计划并保留修改。',
          'Definitions are frozen; continuing discards this plan and keeps your changes.',
        ))
      }</p><pre class="schema-ddl">${
        h(design.plan.split('\n').slice(((design.planPage || 1) - 1) * 3, (design.planPage || 1) * 3).join('\n'))
      }</pre><p>${h(text('DDL 片段', 'DDL fragment'))} ${(design.planPage || 1)} / ${
        Math.ceil(design.plan.split('\n').length / 3)
      } <button type="button" data-schema-example-op="plan-prev"${(design.planPage || 1) === 1 ? ' disabled' : ''}>${
        h(text('上一页', 'Previous'))
      }</button> <button type="button" data-schema-example-op="plan-next"${
        (design.planPage || 1) * 3 >= design.plan.split('\n').length ? ' disabled' : ''
      }>${h(text('下一页', 'Next'))}</button></p></section>`
      : showSections && design.section === 'ddl_preview'
      ? `<section class="schema-section"><h3>${h(text('预览 DDL', 'Preview DDL'))}</h3><p>${
        h(text('请先编辑结构，然后使用工具栏生成计划。', 'Edit the structure, then generate a plan using the toolbar.'))
      }</p></section>`
      : ''
  }${
    showSections && design.section === 'columns'
      ? `<section class="schema-section"><h3>${
        h(text('字段', 'Columns'))
      }</h3><div class="schema-table-scroll"><table class="schema-table schema-columns-table"><thead><tr><th>${
        h(text('名称', 'Name'))
      }</th><th>${h(text('类型', 'Type'))}</th><th>${h(text('长度', 'Length'))}</th><th>${
        h(text('允许 NULL', 'Allow NULL'))
      }</th><th>${h(text('默认值', 'Default'))}</th><th>${h(text('约束', 'Constraint'))}</th><th>${
        h(text('注释', 'Comment'))
      }</th><th>${h(text('操作', 'Actions'))}</th></tr></thead><tbody>${columns}</tbody></table></div>${
        editable
          ? `<div class="schema-add-actions">${
            schemaRowButton('plus', text('新增字段', 'Add column'), 'data-schema-op="add"')
          }</div>`
          : ''
      }${schemaSelectedColumn(tab, editable, errors)}</section>`
      : ''
  }${schemaAdditionalPreview(tab, editable)}</div></div>`;
}

function schemaRefreshFeedback(panel, tab) {
  const design = tab.schemaDesign, errors = schemaErrors(design);
  const preview = panel.querySelector('[data-action="schema-preview"]');
  if (preview) preview.disabled = !schemaHasDraft(tab) || errors.some(item => Object.keys(item).length);
  for (const control of panel.querySelectorAll('[data-schema-field]')) {
    const index = design.columns.findIndex(column => String(column.id) === control.dataset.schemaColumn);
    const key = control.dataset.schemaField, message = errors[index]?.[key];
    if (message) control.setAttribute('aria-invalid', 'true');
    else control.removeAttribute('aria-invalid');
    let error = control.parentElement.querySelector(`[data-schema-error="${key}"]`);
    if (!message) {
      error?.remove();
      continue;
    }
    if (!error) {
      error = document.createElement('span');
      error.className = 'schema-error';
      error.dataset.schemaError = key;
      control.after(error);
    }
    error.textContent = message;
  }
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
  if (
    !column
    || ![
      'name',
      'type',
      'typeParameter',
      'nullable',
      'defaultKind',
      'defaultValue',
      'comment',
      'generated',
      'stored',
      'visible',
      'autoIncrement',
      'charset',
      'collation',
    ].includes(key)
  ) return;
  const oldName = column.name;
  if (key === 'typeParameter') {
    const parts = schemaTypeParts(column.type);
    if (!parts?.length && !parts?.decimal) return;
    column.type = parts.fractionalSeconds && /^0+$/.test(target.value.trim())
      ? `${parts.prefix}${parts.suffix}`
      : `${parts.prefix}(${target.value})${parts.suffix}`;
  } else if (key === 'type') {
    const suffix = schemaTypeParts(column.type)?.suffix || '';
    column.type = target.value + suffix;
  } else {
    column[key] = ['nullable', 'stored', 'visible', 'autoIncrement'].includes(key) ? target.checked : target.value;
    if (key === 'nullable' && !column.nullable && column.defaultKind === 'null') column.defaultKind = 'none';
  }
  if (key === 'name' && oldName !== column.name) {
    design.renameReferences ||= {};
    const refs = oldName
      ? [
        ...design.collections.indexes.flatMap(index =>
          index.parts.filter(part => !part.expression && part.column === oldName).map(part => [part, 'column'])
        ),
        ...design.collections.foreignKeys.flatMap(fk =>
          fk.mappings.flatMap(mapping => [
            ...(mapping.column === oldName ? [[mapping, 'column']] : []),
            ...((fk.refDb || tab.database) === tab.database && fk.refTable === tab.table
                && mapping.refColumn === oldName
              ? [[mapping, 'refColumn']]
              : []),
          ])
        ),
      ]
      : design.renameReferences[column.id] || [];
    for (const [reference, field] of refs) reference[field] = column.name;
    if (column.name) delete design.renameReferences[column.id];
    else design.renameReferences[column.id] = refs;
  }
  if (event.isComposing) return;
  if (target instanceof HTMLSelectElement || target.type === 'checkbox') render();
  else schemaRefreshFeedback(panel, tab);
}

document.addEventListener('input', event => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) schemaInput(event);
});
document.addEventListener('change', event => {
  if (
    event.target instanceof HTMLSelectElement || event.target instanceof HTMLInputElement
    || event.target instanceof HTMLTextAreaElement
  ) schemaInput(event);
});
document.addEventListener('compositionend', event => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) schemaInput(event);
});

document.addEventListener('click', event => {
  const button = event.target.closest('[data-schema-op]');
  if (!button) return;
  const panel = button.closest('#schema-structure-panel');
  const tab = state.tabs.find(item => String(item.id) === panel?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  if (!design) return;
  const index = design.columns.findIndex(column => String(column.id) === button.dataset.columnId);
  if (button.dataset.schemaOp === 'select' && index >= 0) {
    design.selectedColumn = design.columns[index].id;
    render();
    return;
  }
  if (!design.editing || design.plan || state.connectionModes.mysql) return;
  if (button.dataset.schemaOp === 'add') {
    const column = {
      id: Date.now(),
      name: '',
      type: 'VARCHAR(255)',
      nullable: true,
      defaultKind: 'none',
      defaultValue: '',
      comment: '',
      autoIncrement: false,
    };
    design.columns.push(column);
    design.selectedColumn = column.id;
  } else if (index >= 0 && button.dataset.schemaOp === 'primary') {
    const column = design.columns[index];
    let primary = schemaPrimary(design);
    if (primary?.parts.some(part => !part.expression && part.column === column.name)) {
      primary.parts = primary.parts.filter(part => part.expression || part.column !== column.name);
      if (!primary.parts.length) {
        design.collections.indexes = design.collections.indexes.filter(item => item !== primary);
      }
    } else {
      if (!primary) {
        primary = {
          id: ++schemaCollectionId,
          name: '',
          kind: 'PRIMARY',
          visible: true,
          parts: [],
          method: '',
          options: '',
        };
        design.collections.indexes.push(primary);
      }
      primary.parts.push({
        id: ++schemaCollectionId,
        column: column.name,
        expression: '',
        prefix: '',
        direction: 'ASC',
      });
      column.nullable = false;
      if (column.defaultKind === 'null') column.defaultKind = 'none';
    }
  } else if (index >= 0 && button.dataset.schemaOp === 'delete') {
    const column = design.columns[index], primary = schemaPrimary(design);
    if (primary) {
      primary.parts = primary.parts.filter(part => part.expression || part.column !== column.name);
      if (!primary.parts.length) {
        design.collections.indexes = design.collections.indexes.filter(item => item !== primary);
      }
    }
    design.columns.splice(index, 1);
  } else if (index >= 0) {
    const next = index + (button.dataset.schemaOp === 'up' ? -1 : 1);
    if (next >= 0 && next < design.columns.length) {
      [design.columns[index], design.columns[next]] = [design.columns[next], design.columns[index]];
    }
  }
  render();
});

function schemaSelectedColumn(tab, editable, errors) {
  const t = schemaText, design = tab.schemaDesign;
  const column = design.columns.find(item => item.id === design.selectedColumn) || design.columns[0];
  if (!column) return `<p>${t('没有字段。', 'No columns.')}</p>`;
  const attr = key => `data-schema-column="${column.id}" data-schema-field="${key}"${editable ? '' : ' disabled'}`;
  const textField = (key, label) => `<label>${h(label)}<input ${attr(key)} value="${h(column[key] || '')}"></label>`;
  const flag = (key, label, checked, disabled = false) =>
    `<label class="advanced-switch"><span>${h(label)}</span><input type="checkbox" role="switch" ${attr(key)}${
      checked ? ' checked' : ''
    }${disabled ? ' disabled' : ''}></label>`;
  return `<div class="schema-column-properties"><h3>${
    t('选中字段的扩展属性', 'Selected column advanced properties')
  } · ${h(column.name || t('未命名字段', 'Unnamed column'))}</h3><div class="fields">${
    textField('charset', t('字符集', 'Character set'))
  }${textField('collation', t('排序规则', 'Collation'))}<label>${t('生成表达式', 'Generated expression')}<textarea ${
    attr('generated')
  } rows="2">${h(column.generated || '')}</textarea></label></div><div class="schema-properties-flags">${
    flag('autoIncrement', t('自增', 'Auto increment'), column.autoIncrement)
  }${flag('visible', t('可见', 'Visible'), column.visible !== false)}${
    flag('stored', t('存储生成列', 'Stored generated column'), column.stored)
  }</div></div>`;
}

function schemaAdditionalPreview(tab, editable) {
  const t = schemaText;
  const s = tab.schemaExamples ||= {
    object: 'table',
    status: 'complete',
    page: 1,
    indexName: 'idx_status',
    indexKind: 'INDEX',
    indexColumn: 'status',
    expression: '',
    prefix: '',
    descending: false,
    fkName: 'fk_customer',
    refTable: 'customers',
    refColumn: 'id',
    onDelete: 'RESTRICT',
    checkName: 'chk_total',
    checkExpr: 'total >= 0',
    enabled: true,
    engine: 'InnoDB',
    charset: 'utf8mb4',
    collation: 'utf8mb4_0900_ai_ci',
    options: '',
    generated: '',
    stored: false,
    visible: true,
    autoIncrement: false,
    viewName: 'active_orders',
    select: "SELECT * FROM orders WHERE status = 'active'",
    algorithm: 'UNDEFINED',
    security: 'DEFINER',
    triggerName: 'orders_before_insert',
    timing: 'BEFORE',
    event: 'INSERT',
    body: 'SET NEW.created_at = NOW();',
    order: 'NONE',
    orderTarget: '',
  };
  tab.schemaExamplesOriginal ||= { ...s };
  const frozen = !!tab.schemaDesign.plan || state.connectionModes.mysql;
  const field = (key, label, long = false) =>
    `<label>${h(label)}${
      long
        ? `<textarea data-schema-example="${key}" rows="3"${frozen ? ' disabled' : ''}>${h(s[key] || '')}</textarea>`
        : `<input data-schema-example="${key}" value="${h(s[key] || '')}"${frozen ? ' disabled' : ''}>`
    }</label>`;
  const select = (key, label, choices) =>
    `<label>${h(label)}<select data-schema-example="${key}"${
      (frozen && key !== 'status') || (key === 'object' && tab.schemaCreateKind) ? ' disabled' : ''
    }>${
      choices.map(([v, title]) => `<option value="${v}"${s[key] === v ? ' selected' : ''}>${h(title)}</option>`).join(
        '',
      )
    }</select></label>`;
  const statuses = [
    ['complete', t('元数据完整', 'Metadata complete')],
    ['incomplete', t('元数据不完整', 'Metadata incomplete')],
    ['unknown', t('能力未知', 'Capabilities unknown')],
    ['restore', t('恢复草稿', 'Restore draft')],
    ['confirm', t('最终确认', 'Final confirmation')],
    ['partial', t('部分生效', 'Partially applied')],
    ['result-unknown', t('结果未知', 'Unknown result')],
    ['verification_mismatch', t('读回结果不一致', 'Verification mismatch')],
    ['loading', t('加载中', 'Loading')],
    ['empty', t('空列表', 'Empty list')],
    ['failure', t('加载失败', 'Load failed')],
  ];
  const notice = {
    incomplete: t(
      '元数据不完整；缺失的定义不能推断，需重新读取。',
      'Metadata is incomplete; missing definitions cannot be inferred. Re-read the metadata.',
    ),
    unknown: t(
      '服务器版本能力未知；受限功能不能准备。',
      'Server capabilities are unknown; restricted operations cannot be prepared.',
    ),
    restore: t(
      '恢复前须重新读取定义并确认基线；保留本地草稿。',
      'Re-read the definition and confirm the baseline before restoring the local draft.',
    ),
    confirm: t(
      '冻结目标：commerce.orders；删除字段可能丢失数据且无法自动恢复。此设计不执行变更。',
      'Frozen target: commerce.orders; dropping columns may lose data and cannot be automatically recovered. This design does not execute changes.',
    ),
    partial: t(
      '步骤 1 已生效，步骤 2 失败；请读回定义并核对后恢复。',
      'Step 1 applied, step 2 failed. Read back and inspect the definition before recovery.',
    ),
    'result-unknown': t(
      '结果未知；先核对数据库和执行记录，再解除等待，不会自动重放。',
      'Result unknown. Inspect the database and execution record before clearing the wait; no automatic replay.',
    ),
    verification_mismatch: t(
      '读回结果与目标不一致；请核对目标结构和 DDL，重新读取后恢复。不会自动重试 DDL。',
      'The read-back differs from the target. Compare the target structure and DDL, then re-read to recover. DDL is never retried automatically.',
    ),
    loading: t('正在加载下一页…', 'Loading next page…'),
    empty: t('没有触发器。', 'No triggers.'),
    failure: t('读取失败；保留已读取内容并重试。', 'Read failed; retain loaded content and retry.'),
  };
  let content;
  if (s.object === 'view') {
    content = `<section class="schema-section"><h3>${t('视图定义', 'View definition')}</h3><div class="fields">${
      field('viewName', t('视图名称', 'View name'))
    }${
      select('algorithm', t('算法', 'Algorithm'), [['UNDEFINED', 'UNDEFINED'], ['MERGE', 'MERGE'], [
        'TEMPTABLE',
        'TEMPTABLE',
      ]])
    }${select('security', t('安全属性', 'Security'), [['DEFINER', 'DEFINER'], ['INVOKER', 'INVOKER']])}</div>${
      field('select', 'SELECT', true)
    }</section>`;
  } else if (s.object === 'trigger') {
    content = `<section class="schema-section"><h3>${t('触发器定义', 'Trigger definition')}</h3><div class="fields">${
      field('triggerName', t('名称', 'Name'))
    }${select('timing', t('时机', 'Timing'), [['BEFORE', 'BEFORE'], ['AFTER', 'AFTER']])}${
      select('event', t('事件', 'Event'), [['INSERT', 'INSERT'], ['UPDATE', 'UPDATE'], ['DELETE', 'DELETE']])
    }${
      select('order', t('顺序', 'Order'), [['NONE', t('无', 'None')], ['FOLLOWS', 'FOLLOWS'], ['PRECEDES', 'PRECEDES']])
    }${field('orderTarget', t('相对触发器', 'Relative trigger'))}</div>${
      field('body', t('触发器正文', 'Trigger body'), true)
    }</section>`;
  } else if (s.object === 'trigger-list') {
    if (!['loading', 'empty', 'failure'].includes(s.status)) tab.schemaTriggerListPage = s.page;
    if (s.status === 'empty') tab.schemaTriggerListPage = null;
    const displayedPage = ['loading', 'failure'].includes(s.status)
      ? tab.schemaTriggerListPage
      : s.status === 'empty'
      ? null
      : s.page;
    content = `<section class="schema-section"><h3>${t('触发器列表示例', 'Sample triggers')}</h3>${
      displayedPage != null
        ? `<p>${h(tab.table)}${
          displayedPage === 1 ? '_before_insert · BEFORE INSERT' : '_after_update · AFTER UPDATE'
        }</p>`
        : s.status === 'failure' || s.status === 'loading'
        ? `<p>${t('尚未成功读取列表。', 'The list has not been loaded yet.')}</p>`
        : ''
    }${
      s.status === 'failure'
        ? `<p class="notice">${
          t('读取失败；保留先前结果。', 'Load failed; previous results are retained.')
        } <button type="button" data-schema-example-op="retry-triggers">${
          t('模拟重试读取', 'Simulate retry')
        }</button></p>`
        : ''
    }<p>${t('第', 'Page')} ${displayedPage ?? s.page} · ${
      t('总数未知', 'Total unknown')
    } <button type="button" data-schema-example-op="previous"${
      s.page === 1 || ['loading', 'failure'].includes(s.status) ? ' disabled' : ''
    }>${t('上一页', 'Previous')}</button> <button type="button" data-schema-example-op="next"${
      s.page === 2 || ['loading', 'empty', 'failure'].includes(s.status) ? ' disabled' : ''
    }>${t('下一页', 'Next')}</button></p><button type="button" data-schema-example-op="return-table">${
      t('返回表结构', 'Back to table structure')
    }</button></section>`;
  } else {content = ['columns', 'ddl_preview'].includes(tab.schemaDesign.section)
      ? ''
      : tab.schemaDesign.section === 'options'
      ? `<section class="schema-section"><h3>${
        t('表选项', 'Table options')
      }</h3><div class="schema-table-scroll"><table class="schema-table schema-options"><tbody>${
        [['engine', t('引擎', 'Engine')], ['charset', t('字符集', 'Character set')], [
          'collation',
          t('排序规则', 'Collation'),
        ], ['options', t('其他选项', 'Other options')]].map(([key, label]) =>
          `<tr><th scope="row">${h(label)}</th><td><input data-schema-example="${key}" aria-label="${
            h(label)
          }" value="${h(s[key] || '')}"${frozen ? ' disabled' : ''}></td></tr>`
        ).join('')
      }</tbody></table></div></section>`
      : schemaCollections(tab, !editable);}
  const destructive = tab.schemaDesign.original.some(original =>
    !tab.schemaDesign.columns.some(column => column.id === original.id)
  );
  const target = s.object === 'view'
    ? s.viewName
    : s.object === 'trigger'
    ? s.triggerName
    : s.object === 'new-table'
    ? s.tableName || 'new_table'
    : tab.table;
  return `<section class="schema-section schema-example"><div class="fields">${
    select('status', t('元数据与结果示例', 'Metadata and result sample'), statuses)
  }${s.object === 'new-table' ? field('tableName', t('新表名', 'New table name')) : ''}</div>${
    s.status !== 'confirm' && notice[s.status] ? `<p class="notice">${notice[s.status]}</p>` : ''
  }${
    s.status === 'confirm'
      ? `<p class="notice">${t('冻结目标', 'Frozen target')}: ${h(tab.database)}.${h(target)} · ${
        destructive
          ? t(
            '删除字段可能丢失数据，无法自动恢复。',
            'Dropping a column may lose data and cannot be automatically recovered.',
          )
          : t(
            '创建或修改定义；实际影响以真实计划为准。',
            'Create or change the definition; actual impact depends on the real plan.',
          )
      }</p><button class="button${destructive ? ' danger' : ' primary'}" disabled>${
        t('确认执行', 'Confirm execution')
      }</button><p>${t('设计示例不可执行。', 'Execution is unavailable in this design sample.')}</p>`
      : ''
  }${
    ['restore', 'incomplete', 'unknown'].includes(s.status)
      ? `<button class="button" data-schema-example-op="baseline">${
        t('模拟重新读取并确认基线', 'Simulate re-reading and confirming baseline')
      }</button>`
      : ''
  }${
    s.status === 'result-unknown'
      ? `<button class="button" data-schema-example-op="checked">${
        t('已核对目标与执行记录，解除等待', 'Target and execution record checked; clear wait')
      }</button>`
      : ''
  }${
    s.status === 'verification_mismatch'
      ? `<section><h3>${t('核对目标结构与 DDL', 'Compare target structure and DDL')}</h3><p>${h(tab.database)}.${
        h(target)
      }</p><p>${t('目标字段', 'Target columns')}: ${
        h(tab.schemaDesign.columns.map(column => `${column.name} ${column.type}`).join(', ') || '—')
      }</p>${
        ['table', 'new-table'].includes(s.object)
          ? ''
          : `<pre class="schema-ddl">${h(tab.schemaDesign.plan || '')}</pre>`
      }<p>${
        t(
          '示例未读取真实数据库；请核对实际读回的结构。',
          'This sample has not read a database; inspect the actual read-back structure.',
        )
      }</p><button type="button" data-schema-example-op="reread-mismatch">${
        t('模拟重新读取并恢复编辑', 'Simulate re-read and resume editing')
      }</button></section>`
      : ''
  }${content}<p class="schema-hint">${
    t('设计预览不连接数据库，也不执行查询。', 'The design preview does not connect to a database or execute queries.')
  }</p><button class="button" disabled>${
    t('原定义转查询', 'Send original to query')
  }</button> <button class="button" disabled>${t('预览转查询', 'Send preview to query')}</button></section>`;
}

function schemaExampleInput(event) {
  const el = event.target;
  if (!(el instanceof HTMLElement) || !el.dataset.schemaExample) return;
  const panel = el.closest('#schema-structure-panel');
  const tab = state.tabs.find(item => String(item.id) === panel?.dataset.schemaTab);
  if (!tab?.schemaExamples) return;
  const key = el.dataset.schemaExample;
  if (
    key === 'status' && ['confirm', 'partial', 'result-unknown', 'verification_mismatch'].includes(el.value)
    && !tab.schemaDesign.plan
  ) {
    toast(schemaText('请先准备冻结计划。', 'Prepare a frozen plan first.'));
    render();
    return;
  }
  tab.schemaExamples[key] = el.type === 'checkbox' ? el.checked : el.value;
  if (!['status', 'page'].includes(key)) {
    tab.schemaExamplesDirty = JSON.stringify({ ...tab.schemaExamples, status: 'complete', page: 1 })
      !== JSON.stringify({ ...tab.schemaExamplesOriginal, status: 'complete', page: 1 });
    tab.schemaDesign.editing = true;
  }
  if (el instanceof HTMLSelectElement || el.type === 'checkbox') render();
  else schemaRefreshFeedback(panel, tab);
}
document.addEventListener('input', schemaExampleInput);
document.addEventListener('change', schemaExampleInput);
document.addEventListener('click', event => {
  const el = event.target.closest('[data-schema-example-op]');
  if (!el) return;
  const tab = state.tabs.find(item => String(item.id) === el.closest('#schema-structure-panel')?.dataset.schemaTab);
  if (!tab?.schemaExamples) return;
  if (['baseline', 'checked'].includes(el.dataset.schemaExampleOp)) tab.schemaExamples.status = 'complete';
  else if (el.dataset.schemaExampleOp === 'retry-triggers') tab.schemaExamples.status = 'complete';
  else if (el.dataset.schemaExampleOp === 'reread-mismatch') {
    tab.schemaExamples.status = 'complete';
    tab.schemaDesign.plan = null;
    tab.schemaDesign.editing = true;
  } else if (el.dataset.schemaExampleOp === 'return-table') {
    tab.schemaExamples.object = 'table';
    tab.schemaExamples.status = 'complete';
  } else if (el.dataset.schemaExampleOp.startsWith('plan-')) {
    tab.schemaDesign.planPage = Math.max(
      1,
      Math.min(
        Math.ceil(tab.schemaDesign.plan.split('\n').length / 3),
        (tab.schemaDesign.planPage || 1) + (el.dataset.schemaExampleOp === 'plan-next' ? 1 : -1),
      ),
    );
  } else {tab.schemaExamples.page = Math.max(
      1,
      Math.min(2, tab.schemaExamples.page + (el.dataset.schemaExampleOp === 'next' ? 1 : -1)),
    );}
  render();
});

function schemaOpenExample(kind) {
  if (
    !['new-table', 'view', 'trigger', 'trigger-list'].includes(kind)
    || state.connectionModes.mysql && kind !== 'trigger-list'
  ) return false;
  const tab = state.tabs.find(item => item.id === state.active && !item.query);
  if (!tab) return false;
  const design = schemaState(tab);
  if (design.plan) return false;
  schemaAdditionalPreview(tab, true);
  tab.schemaExamples.object = kind;
  tab.schemaExamplesDirty = kind !== 'trigger-list';
  design.editing = kind !== 'trigger-list';
  state.section = 'structure';
  render();
  return true;
}

let schemaCollectionId = 0;
function schemaCollections(tab, frozen) {
  const t = schemaText, design = schemaState(tab), kind = design.section;
  const titles = { indexes: t('索引', 'Indexes'), foreignKeys: t('外键', 'Foreign keys'), checks: 'CHECK' };
  const items = design.collections[kind], disabled = frozen ? ' disabled' : '';
  const fields = kind === 'indexes'
    ? ['name', 'kind', 'table', 'parts', 'visible']
    : kind === 'foreignKeys'
    ? ['name', 'refTable', 'mappings', 'onUpdate', 'onDelete']
    : ['name', 'expression', 'enabled'];
  const labels = {
    name: t('名称', 'Name'),
    kind: t('类型', 'Kind'),
    table: t('表', 'Table'),
    parts: t('索引列（有序）', 'Index columns (ordered)'),
    visible: t('可见', 'Visible'),
    refTable: t('引用表', 'Referenced table'),
    mappings: t('本地列 / 引用列', 'Local / referenced columns'),
    onUpdate: t('更新时', 'On update'),
    onDelete: t('删除时', 'On delete'),
    expression: t('表达式', 'Expression'),
    enabled: t('启用', 'Enforced'),
  };
  const choices = {
    kind: [
      ['INDEX', t('普通索引', 'Index')],
      ['UNIQUE', t('唯一索引', 'Unique')],
      ['PRIMARY', t('主键', 'Primary key')],
      ['FULLTEXT', 'FULLTEXT'],
      ['SPATIAL', 'SPATIAL'],
    ],
    visible: [['true', t('是', 'Yes')], ['false', t('否', 'No')]],
    enabled: [['true', t('是', 'Yes')], ['false', t('否', 'No')]],
    onUpdate: ['RESTRICT', 'CASCADE', 'SET NULL', 'NO ACTION'].map(value => [value, value]),
    onDelete: ['RESTRICT', 'CASCADE', 'SET NULL', 'NO ACTION'].map(value => [value, value]),
  };
  const localColumns = design.columns.filter(column => column.name).map(column => column.name);
  const attrs = (item, key, extra = '') =>
    `data-schema-collection-field="${key}" data-collection="${kind}" data-collection-id="${item.id}" ${extra}${disabled}`;
  const select = (item, key, value, values, label, extra = '') => {
    const missing = value && !values.some(([option]) => option === String(value));
    return `<select ${attrs(item, key, extra)} aria-label="${h(label)}">${
      missing
        ? `<option selected value="${h(value)}">${h(value)} · ${
          h(t('示例元数据中不可用', 'Unavailable in sample metadata'))
        }</option>`
        : ''
    }${
      values.map(([option, title]) =>
        `<option value="${h(option)}"${String(value) === option ? ' selected' : ''}${
          key === 'kind' && option === 'PRIMARY' && schemaPrimary(design) && schemaPrimary(design) !== item
            ? ' disabled'
            : ''
        }>${h(title)}</option>`
      ).join('')
    }</select>`;
  };
  const fieldChoices = values => [['', t('选择字段', 'Select column')], ...values.map(value => [value, value])];
  const subActions = (item, list, part, position, type) =>
    ['up', 'down', 'delete'].map(op =>
      schemaRowButton(
        op === 'up' ? 'chev-up' : op === 'down' ? 'chev' : 'trash',
        op === 'up' ? t('上移', 'Move up') : op === 'down' ? t('下移', 'Move down') : t('删除', 'Delete'),
        `data-schema-${type}-action="${op}" data-collection-id="${item.id}" data-${type}-id="${part.id}"`,
        frozen || (op === 'up' && position === 0) || (op === 'down' && position === list.length - 1),
      )
    ).join('');
  const rows = items.map((item, position) => {
    const currentDb = item.refDb || tab.database;
    const tables = Object.keys(databases[currentDb] || {});
    const referenced = item.refTable && currentDb === tab.database && item.refTable === tab.table
      ? localColumns
      : Object.keys(databases[currentDb]?.[item.refTable]?.[0] || {});
    const cells = fields.map(key => {
      const value = item[key] ?? '';
      if (key === 'table') return `<td>${h(tab.table || tab.schemaExamples?.tableName || t('新表', 'New table'))}</td>`;
      if (key === 'parts') {
        return `<td>${
          item.parts.map((part, index) =>
            `<div class="schema-inline-parts">${
              select(
                item,
                'column',
                part.column,
                fieldChoices(localColumns),
                t('索引列', 'Index column'),
                `data-part-id="${part.id}"`,
              )
            }${subActions(item, item.parts, part, index, 'part')}</div>`
          ).join('')
        }${
          schemaRowButton(
            'plus',
            t('新增索引项', 'Add index part'),
            `data-schema-part-action="add" data-collection-id="${item.id}"`,
            frozen,
          )
        }</td>`;
      }
      if (key === 'refTable') {
        return `<td>${
          select(
            item,
            key,
            value,
            [['', t('选择表', 'Select table')], ...tables.map(name => [name, name])],
            labels[key],
          )
        }</td>`;
      }
      if (key === 'mappings') {
        return `<td>${
          item.mappings.map((mapping, index) =>
            `<div class="schema-inline-parts">${
              select(
                item,
                'column',
                mapping.column,
                fieldChoices(localColumns),
                t('本地列', 'Local column'),
                `data-mapping-id="${mapping.id}"`,
              )
            }${
              select(
                item,
                'refColumn',
                mapping.refColumn,
                fieldChoices(referenced),
                t('引用列', 'Referenced column'),
                `data-mapping-id="${mapping.id}"`,
              )
            }${subActions(item, item.mappings, mapping, index, 'mapping')}</div>`
          ).join('')
        }${
          schemaRowButton(
            'plus',
            t('新增映射', 'Add mapping'),
            `data-schema-mapping-action="add" data-collection-id="${item.id}"`,
            frozen,
          )
        }</td>`;
      }
      if (choices[key]) return `<td>${select(item, key, value, choices[key], labels[key])}</td>`;
      return `<td>${
        key === 'expression'
          ? `<textarea ${attrs(item, key)} aria-label="${h(labels[key])}" rows="2">${h(value)}</textarea>`
          : `<input ${attrs(item, key)} aria-label="${h(labels[key])}" value="${h(value)}">`
      }</td>`;
    }).join('');
    const rowAttr = `data-collection="${kind}" data-collection-id="${item.id}"`;
    const actions = ['up', 'down', 'delete'].map(op =>
      schemaRowButton(
        op === 'up' ? 'chev-up' : op === 'down' ? 'chev' : 'trash',
        op === 'up' ? t('上移', 'Move up') : op === 'down' ? t('下移', 'Move down') : t('删除', 'Delete'),
        `data-schema-collection-action="${op}" ${rowAttr}`,
        frozen || (op === 'up' && position === 0) || (op === 'down' && position === items.length - 1),
      )
    ).join('');
    return `<tr data-schema-collection-row ${rowAttr}${
      item.id === design.collectionSelection[kind] ? ' class="schema-row-selected"' : ''
    }><td><button type="button" data-schema-collection-action="select" ${rowAttr} aria-label="${
      h(t('选择', 'Select') + ' ' + (item.name || titles[kind]))
    }" aria-pressed="${item.id === design.collectionSelection[kind]}">${
      item.id === design.collectionSelection[kind] ? '●' : '○'
    }</button></td>${cells}<td class="schema-row-actions">${actions}</td></tr>`;
  }).join('');
  return `<section class="schema-section"><h3>${h(titles[kind])}</h3><p class="schema-hint">${
    h(t(
      '表和列候选来自本地示例元数据；不可用的已有值会保留。',
      'Table and column choices come from local sample metadata; unavailable existing values are retained.',
    ))
  }</p>${
    items.length
      ? `<div class="schema-table-scroll"><table class="schema-table schema-collection-grid"><thead><tr><th>${
        h(t('选择', 'Select'))
      }</th>${fields.map(key => `<th>${h(labels[key])}</th>`).join('')}<th>${
        h(t('操作', 'Actions'))
      }</th></tr></thead><tbody>${rows}</tbody></table></div>`
      : `<p>${t('没有定义。', 'No definitions.')}</p>`
  }<div class="schema-add-actions">${
    schemaRowButton(
      'plus',
      kind === 'indexes'
        ? t('新增索引', 'Add index')
        : kind === 'foreignKeys'
        ? t('新增外键', 'Add foreign key')
        : t('新增 CHECK', 'Add CHECK'),
      `data-schema-collection-action="add" data-collection="${kind}"`,
      frozen,
    )
  }</div>${schemaCollectionDetails(tab, frozen)}</section>`;
}

function schemaCollectionDetails(tab, frozen) {
  const t = schemaText, design = tab.schemaDesign, kind = design.section;
  const item = design.collections[kind]?.find(entry => entry.id === design.collectionSelection[kind]);
  if (!item || kind === 'checks') return '<div class="schema-collection-details"></div>';
  const disabled = frozen ? ' disabled' : '';
  const attr = (key, extra = '') =>
    `data-schema-collection-field="${key}" data-collection="${kind}" data-collection-id="${item.id}" ${extra}${disabled}`;
  if (kind === 'foreignKeys') {
    return `<div class="schema-collection-details schema-column-properties"><label>${
      h(t('引用库（留空为当前库）', 'Referenced database (empty for current)'))
    }<input ${attr('refDb')} value="${h(item.refDb || '')}"></label></div>`;
  }
  const parts = item.parts.map((part, index) =>
    `<fieldset><legend>${t('索引项', 'Index part')} ${index + 1}</legend><div class="fields"><label>${
      t('表达式', 'Expression')
    }<textarea ${attr('expression', `data-part-id="${part.id}"`)} rows="2">${
      h(part.expression)
    }</textarea></label><label>${t('前缀长度', 'Prefix length')}<input ${
      attr('prefix', `data-part-id="${part.id}"`)
    } value="${h(part.prefix)}"></label><label>${t('方向', 'Direction')}<select ${
      attr('direction', `data-part-id="${part.id}"`)
    }><option value="ASC"${part.direction === 'ASC' ? ' selected' : ''}>${
      t('升序', 'Ascending')
    }</option><option value="DESC"${part.direction === 'DESC' ? ' selected' : ''}>${
      t('降序', 'Descending')
    }</option></select></label></div></fieldset>`
  ).join('');
  return `<div class="schema-collection-details schema-column-properties"><h3>${
    t('选中项高级属性', 'Selected item advanced properties')
  } · ${h(item.name || String(item.id))}</h3><div class="fields">${parts}<label>${t('方法', 'Method')}<select ${
    attr('method')
  }>${
    [['', t('默认', 'Default')], ['BTREE', 'BTREE'], ['HASH', 'HASH']].map(([value, label]) =>
      `<option value="${value}"${item.method === value ? ' selected' : ''}>${h(label)}</option>`
    ).join('')
  }</select></label><label>${t('选项', 'Options')}<input ${attr('options')} value="${
    h(item.options || '')
  }"></label></div></div>`;
}

function schemaCollectionInput(event) {
  const el = event.target;
  if (!(el instanceof HTMLElement) || !el.dataset.schemaCollectionField) return;
  const kind = el.dataset.collection;
  const tab = state.tabs.find(item => String(item.id) === el.closest('#schema-structure-panel')?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  if (!design?.editing || !design.collections[kind] || design.plan || state.connectionModes.mysql) return;
  const item = design.collections[kind].find(entry => String(entry.id) === el.dataset.collectionId);
  if (!item) return;
  design.collectionSelection[kind] = item.id;
  const key = el.dataset.schemaCollectionField;
  const target = el.dataset.partId
    ? item.parts?.find(part => String(part.id) === el.dataset.partId)
    : el.dataset.mappingId
    ? item.mappings?.find(mapping => String(mapping.id) === el.dataset.mappingId)
    : item;
  if (!target) return;
  target[key] = ['visible', 'enabled'].includes(key) ? el.value === 'true' : el.value;
  if (el.dataset.partId && key === 'expression' && el.value.trim()) {
    target.column = '';
    target.prefix = '';
    const fieldset = el.closest('fieldset');
    const gridColumn = el.closest('#schema-structure-panel').querySelector(
      `[data-schema-collection-field="column"][data-collection-id="${item.id}"][data-part-id="${target.id}"]`,
    );
    if (gridColumn) gridColumn.value = '';
    const prefix = fieldset?.querySelector('[data-schema-collection-field="prefix"]');
    if (prefix) prefix.value = '';
  } else if (el.dataset.partId && key === 'column' && el.value) {
    target.expression = '';
  }
  if (kind === 'indexes') schemaRequirePrimaryNotNull(design);
  design.editing = true;
  if ((el instanceof HTMLSelectElement || key === 'refDb') && event.type === 'change') render();
  else schemaRefreshFeedback(el.closest('#schema-structure-panel'), tab);
}
document.addEventListener('input', schemaCollectionInput);
document.addEventListener('change', schemaCollectionInput);
document.addEventListener('click', event => {
  const el = event.target.closest('[data-schema-collection-action]');
  if (!el) return;
  const tab = state.tabs.find(item => String(item.id) === el.closest('#schema-structure-panel')?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  const kind = el.dataset.collection;
  if (!design?.collections[kind]) return;
  if (el.dataset.schemaCollectionAction === 'select') {
    design.collectionSelection[kind] = Number(el.dataset.collectionId);
    render();
    return;
  }
  if (!design.editing || design.plan || state.connectionModes.mysql) return;
  if (el.dataset.schemaCollectionAction === 'add') {
    const id = ++schemaCollectionId;
    const column = design.columns[0]?.name || '';
    const item = kind === 'indexes'
      ? {
        id,
        name: '',
        kind: 'INDEX',
        visible: true,
        parts: [{ id: ++schemaCollectionId, column, expression: '', prefix: '', direction: 'ASC' }],
        method: '',
        options: '',
      }
      : kind === 'foreignKeys'
      ? {
        id,
        name: '',
        refDb: '',
        refTable: '',
        mappings: [{ id: ++schemaCollectionId, column, refColumn: '' }],
        onDelete: 'RESTRICT',
        onUpdate: 'RESTRICT',
      }
      : { id, name: '', expression: '', enabled: true };
    design.collections[kind].push(item);
    design.collectionSelection[kind] = id;
  } else {
    const position = design.collections[kind].findIndex(item => String(item.id) === el.dataset.collectionId);
    if (position < 0) return;
    if (el.dataset.schemaCollectionAction === 'delete') {
      const removed = design.collections[kind].splice(position, 1)[0];
      if (design.collectionSelection[kind] === removed.id) {
        design.collectionSelection[kind] = design.collections[kind][0]?.id;
      }
    } else {
      const next = position + (el.dataset.schemaCollectionAction === 'up' ? -1 : 1);
      if (next >= 0 && next < design.collections[kind].length) {
        [design.collections[kind][position], design.collections[kind][next]] = [
          design.collections[kind][next],
          design.collections[kind][position],
        ];
      }
    }
  }
  design.editing = true;
  render();
});

document.addEventListener('click', event => {
  const button = event.target.closest('[data-schema-part-action]');
  if (!button) return;
  const tab = state.tabs.find(item => String(item.id) === button.closest('#schema-structure-panel')?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  const index = design?.collections.indexes.find(item => String(item.id) === button.dataset.collectionId);
  if (!index || !design.editing || design.plan || state.connectionModes.mysql) return;
  design.collectionSelection.indexes = index.id;
  if (button.dataset.schemaPartAction === 'add') {
    index.parts.push({
      id: ++schemaCollectionId,
      column: design.columns.find(column => !index.parts.some(part => part.column === column.name))?.name || '',
      expression: '',
      prefix: '',
      direction: 'ASC',
    });
  } else {
    const position = index.parts.findIndex(part => String(part.id) === button.dataset.partId);
    if (position < 0) return;
    if (button.dataset.schemaPartAction === 'delete') index.parts.splice(position, 1);
    else {
      const next = position + (button.dataset.schemaPartAction === 'up' ? -1 : 1);
      if (next >= 0 && next < index.parts.length) {
        [index.parts[position], index.parts[next]] = [index.parts[next], index.parts[position]];
      }
    }
  }
  schemaRequirePrimaryNotNull(design);
  design.editing = true;
  render();
});

document.addEventListener('click', event => {
  const button = event.target.closest('[data-schema-mapping-action]');
  if (!button) return;
  const tab = state.tabs.find(item => String(item.id) === button.closest('#schema-structure-panel')?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  const fk = design?.collections.foreignKeys.find(item => String(item.id) === button.dataset.collectionId);
  if (!fk || !design.editing || design.plan || state.connectionModes.mysql) return;
  design.collectionSelection.foreignKeys = fk.id;
  if (button.dataset.schemaMappingAction === 'add') {
    fk.mappings.push({ id: ++schemaCollectionId, column: design.columns[0]?.name || '', refColumn: '' });
  } else {
    const position = fk.mappings.findIndex(mapping => String(mapping.id) === button.dataset.mappingId);
    if (position < 0) return;
    if (button.dataset.schemaMappingAction === 'delete') fk.mappings.splice(position, 1);
    else {
      const next = position + (button.dataset.schemaMappingAction === 'up' ? -1 : 1);
      if (next >= 0 && next < fk.mappings.length) {
        [fk.mappings[position], fk.mappings[next]] = [fk.mappings[next], fk.mappings[position]];
      }
    }
  }
  design.editing = true;
  render();
});

document.addEventListener('click', event => {
  const button = event.target.closest('[data-schema-section]');
  if (!button) return;
  const tab = state.tabs.find(item => String(item.id) === button.closest('#schema-structure-panel')?.dataset.schemaTab);
  if (!tab?.schemaDesign) return;
  tab.schemaDesign.section = button.dataset.schemaSection;
  render();
});
document.addEventListener('keydown', event => {
  const button = event.target.closest('[data-schema-section]');
  if (!button || !['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
  const buttons = [...button.parentElement.querySelectorAll('[data-schema-section]')];
  const current = buttons.indexOf(button);
  const next = event.key === 'Home'
    ? 0
    : event.key === 'End'
    ? buttons.length - 1
    : (current + (['ArrowRight', 'ArrowDown'].includes(event.key) ? 1 : -1) + buttons.length) % buttons.length;
  event.preventDefault();
  buttons[next].focus();
});

function schemaSelectGridRow(row) {
  const panel = row.closest('#schema-structure-panel');
  const tab = state.tabs.find(item => String(item.id) === panel?.dataset.schemaTab);
  const design = tab?.schemaDesign;
  if (!design) return;
  const editable = design.editing && !design.plan && !state.connectionModes.mysql;
  if (row.hasAttribute('data-schema-row')) {
    const column = design.columns.find(item => String(item.id) === row.dataset.schemaRow);
    if (!column || design.selectedColumn === column.id) return;
    design.selectedColumn = column.id;
    panel.querySelectorAll('[data-schema-row]').forEach(candidate => {
      const selected = candidate === row;
      candidate.classList.toggle('schema-row-selected', selected);
      candidate.querySelector('[data-schema-op="select"]')?.setAttribute('aria-pressed', String(selected));
    });
    const properties = panel.querySelector('.schema-column-properties');
    if (properties) properties.outerHTML = schemaSelectedColumn(tab, editable, schemaErrors(design));
  } else {
    const kind = row.dataset.collection;
    const item = design.collections[kind]?.find(entry => String(entry.id) === row.dataset.collectionId);
    if (!item || design.collectionSelection[kind] === item.id) return;
    design.collectionSelection[kind] = item.id;
    panel.querySelectorAll('[data-schema-collection-row]').forEach(candidate => {
      const selected = candidate === row;
      candidate.classList.toggle('schema-row-selected', selected);
      const button = candidate.querySelector('[data-schema-collection-action="select"]');
      if (button) {
        button.setAttribute('aria-pressed', String(selected));
        button.textContent = selected ? '●' : '○';
      }
    });
    const details = panel.querySelector('.schema-collection-details');
    if (details) details.outerHTML = schemaCollectionDetails(tab, !editable);
  }
}

document.addEventListener('click', event => {
  if (event.target.closest('input, select, button, textarea, a, [contenteditable]')) return;
  const row = event.target.closest('[data-schema-row], [data-schema-collection-row]');
  if (row) schemaSelectGridRow(row);
});

document.addEventListener('focusin', event => {
  if (!event.target.matches('input, select, textarea')) return;
  const row = event.target.closest('[data-schema-row], [data-schema-collection-row]');
  if (row) schemaSelectGridRow(row);
});

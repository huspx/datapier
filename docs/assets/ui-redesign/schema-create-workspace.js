'use strict';

// 每个创建标签页独立保存本地定义，不会创建数据库对象。
function schemaCreateWorkspace(kind) {
  if (!['new-table', 'view', 'trigger'].includes(kind) || state.connectionModes.mysql) return false;
  const database = state.database;
  if (!database || !databases[database]) return false;

  const labels = {
    'new-table': schemaText('新建表', 'New table'),
    view: schemaText('新建视图', 'New view'),
    trigger: schemaText('新建触发器', 'New trigger'),
  };
  const examples = {
    object: kind,
    status: 'complete',
    page: 1,
    tableName: '',
    targetTable: '',
    indexName: '',
    indexKind: 'INDEX',
    indexColumn: '',
    expression: '',
    prefix: '',
    descending: false,
    fkName: '',
    refTable: '',
    refColumn: '',
    onDelete: 'RESTRICT',
    checkName: '',
    checkExpr: '',
    enabled: true,
    engine: 'InnoDB',
    charset: 'utf8mb4',
    collation: 'utf8mb4_0900_ai_ci',
    options: '',
    advancedColumn: '',
    generated: '',
    stored: false,
    visible: true,
    autoIncrement: false,
    viewName: '',
    select: '',
    algorithm: 'UNDEFINED',
    security: 'DEFINER',
    triggerName: '',
    timing: 'BEFORE',
    event: 'INSERT',
    body: '',
    order: 'NONE',
    orderTarget: '',
  };
  const columns = kind === 'new-table'
    ? [{
      id: 0,
      name: '',
      type: 'VARCHAR(255)',
      nullable: true,
      defaultKind: 'none',
      defaultValue: '',
      comment: '',
      primary: false,
      autoIncrement: false,
    }]
    : [];
  const tab = {
    id: state.nextTab++,
    provider: 'mysql',
    database,
    table: '',
    title: labels[kind],
    schemaCreateKind: kind,
    schemaDesign: { original: [], columns, editing: true, plan: null },
    schemaExamples: examples,
    schemaExamplesOriginal: { ...examples },
    schemaExamplesDirty: kind !== 'new-table',
  };
  state.tabs.push(tab);
  state.active = tab.id;
  state.provider = 'mysql';
  state.table = '';
  state.section = 'structure';
  state.menu = null;
  render();
  return true;
}

function schemaCreateWorkspacePanel(tab) {
  // 丢弃设计时恢复 schemaExamples，其中包含触发器目标表。
  if (tab.schemaCreateKind === 'trigger') tab.table = tab.schemaExamples.targetTable || '';
  const target = tab.schemaCreateKind === 'trigger'
    ? `<label class="schema-hint">${h(schemaText('目标表', 'Target table'))} <select data-schema-create-target${
      tab.schemaDesign.plan || state.connectionModes.mysql ? ' disabled' : ''
    }><option value="">${h(schemaText('选择表', 'Select a table'))}</option>${
      Object.keys(databases[tab.database] || {}).map(name =>
        `<option value="${h(name)}"${tab.table === name ? ' selected' : ''}>${h(name)}</option>`
      ).join('')
    }</select></label>`
    : '';
  let panel = schemaStructurePanel(tab);
  if (tab.schemaCreateKind === 'trigger' && !tab.table) {
    panel = panel.replace('data-action="schema-preview"', 'data-action="schema-preview" disabled');
  }
  return `<div class="subnav"><span>${h(tab.title)} · ${
    h(tab.database)
  }</span><span class="spacer"></span>${target}</div>${panel}`;
}

document.addEventListener('change', event => {
  const target = event.target;
  if (!(target instanceof HTMLSelectElement) || !target.hasAttribute('data-schema-create-target')) return;
  const tab = state.tabs.find(item => item.id === state.active && item.schemaCreateKind === 'trigger');
  if (!tab || tab.schemaDesign.plan || state.connectionModes.mysql) return;
  tab.table = target.value;
  tab.schemaExamples.targetTable = target.value;
  tab.schemaExamplesDirty = JSON.stringify({ ...tab.schemaExamples, status: 'complete', page: 1 })
    !== JSON.stringify({ ...tab.schemaExamplesOriginal, status: 'complete', page: 1 });
  render();
});

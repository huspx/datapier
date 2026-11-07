'use strict';
const sqlKeywords = new Set(
  'SELECT FROM WHERE ORDER BY LIMIT OFFSET AS ASC DESC JOIN LEFT RIGHT INNER OUTER ON GROUP HAVING AND OR NOT IN IS NULL LIKE BETWEEN DISTINCT INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE ALTER DROP IF EXISTS PRIMARY KEY AUTO_INCREMENT DEFAULT CURRENT_TIMESTAMP ENGINE CHARSET BIGINT VARCHAR DECIMAL DATETIME COUNT SUM CASE WHEN THEN ELSE END UNION ALL TRUE FALSE'
    .split(' '),
);
// 原型只展示普通 MySQL 语句的边界；生产路径复用 Rust tokenizer 和真实会话 sql_mode。
function scanSql(sql) {
  const tokens = [];
  let offset = 0, error = '';
  while (offset < sql.length) {
    const start = offset;
    let kind = '', closed = true;
    const rest = sql.slice(offset);
    if (/^\s/.test(rest)) offset += rest.match(/^\s+/)[0].length;
    else if (rest[0] === '#' || /^--[\s\u0000-\u001f]/.test(rest)) {
      kind = 'comment';
      const newline = sql.indexOf('\n', offset);
      offset = newline === -1 ? sql.length : newline;
    } else if (rest.startsWith('/*')) {
      kind = 'comment';
      const end = sql.indexOf('*/', offset + 2);
      if (/^\/\*(?:[!+]|[Mm]!)/.test(rest)) {
        error ||= t('executableComment');
      }
      if (end === -1) {
        offset = sql.length;
        error ||= t('unclosedComment');
      } else {
        if (sql.slice(offset + 2, end).includes('/*')) {
          error ||= t('nestedComment');
        }
        offset = end + 2;
      }
    } else if (["'", '"', '`'].includes(rest[0])) {
      const quote = rest[0];
      kind = quote === '`' ? 'identifier' : 'string';
      offset++;
      closed = false;
      while (offset < sql.length) {
        if (sql[offset] === quote) {
          if (sql[offset + 1] === quote) offset += 2;
          else {
            offset++;
            closed = true;
            break;
          }
        } else if (sql[offset] === '\\' && quote !== '`') {
          offset = Math.min(sql.length, offset + 2);
        } else offset++;
      }
      if (!closed) error ||= t('unclosedQuote');
    } else if (/^[A-Za-z_$\u0080-\uffff]/.test(rest)) {
      const word = rest.match(/^[A-Za-z0-9_$\u0080-\uffff]+/)[0];
      offset += word.length;
      kind = sqlKeywords.has(word.toUpperCase()) ? 'keyword' : 'word';
    } else if (/^\d/.test(rest)) {
      kind = 'number';
      offset += rest.match(
        /^(?:0x[\da-f]+|0b[01]+|\d+(?:\.\d*)?(?:e[+-]?\d+)?)/i,
      )[0].length;
    } else {
      offset++;
      if (rest[0] === '\\') {
        error ||= t('clientEscape');
      }
    }
    tokens.push({
      start,
      end: offset,
      kind,
      text: sql.slice(start, offset),
    });
  }
  const words = tokens.filter(t => ['word', 'keyword', 'identifier'].includes(t.kind)).map(t =>
    t.text.replace(/^`|`$/g, '').toUpperCase()
  );
  if (
    words.some(w => ['DELIMITER', 'SOURCE', 'SQL_MODE'].includes(w))
    || /^\s*\\\./m.test(sql)
  ) error ||= t('clientMode');
  if (
    words.includes('CREATE')
    && words.some(w => ['PROCEDURE', 'FUNCTION', 'TRIGGER', 'EVENT'].includes(w))
  ) error ||= t('storedProgram');
  const statements = [];
  let segment = [];
  const finish = (scopeEnd = sql.length) => {
    const meaningful = segment.filter(t => t.kind !== 'comment' && t.text.trim());
    if (meaningful.length) {
      const start = meaningful[0].start, end = meaningful.at(-1).end;
      statements.push({
        start,
        end,
        scopeStart: segment[0].start,
        scopeEnd,
        line: sql.slice(0, start).split('\n').length,
        lastLine: sql.slice(0, end).split('\n').length,
      });
    }
    segment = [];
  };
  for (const token of tokens) {
    if (!token.kind && token.text === ';') finish(token.start);
    else segment.push(token);
  }
  finish();
  return { tokens, statements: error ? [] : statements, error };
}
// 预览仅接受边界与 MySQL 语句类型均可保守识别的单条语句。
function explainableSelection(sql) {
  const parsed = scanSql(sql);
  if (parsed.error || parsed.statements.length !== 1) return false;
  const meaningful = parsed.tokens.filter(t => t.kind !== 'comment' && t.text.trim());
  const semicolons = meaningful.filter(t => !t.kind && t.text === ';');
  if (
    semicolons.length > 1
    || (semicolons.length && meaningful.at(-1) !== semicolons[0])
  ) return false;
  if (
    parsed.tokens.some(
      t => (t.kind === 'string'
        && (t.text.startsWith('"') || t.text.includes('\\'))),
    )
  ) return false;
  return /^(SELECT|INSERT|REPLACE|UPDATE|DELETE)$/i.test(
    meaningful[0]?.text || '',
  );
}
function highlightSql(sql, parsed = scanSql(sql)) {
  return parsed.tokens.map(token =>
    ['keyword', 'string', 'number', 'comment', 'identifier'].includes(
        token.kind,
      )
      ? `<span class="sql-${token.kind}">${h(token.text)}</span>`
      : h(token.text)
  ).join('');
}
function queryTab() {
  const tab = state.tabs.find(t => t.id === state.active && t.query);
  if (!tab) return null;
  tab.sql ??=
    `-- 最近支付的订单\nSELECT id, order_no, status, total\nFROM \`${tab.database}\`.\`orders\`\nWHERE status = 'paid'\nORDER BY id DESC\nLIMIT 50;\n\n-- 查看付款用户\nSELECT id, name, email\nFROM \`${tab.database}\`.\`customers\`\nWHERE tier = 'pro';`;
  tab.sqlStart ??= 0;
  tab.sqlEnd ??= 0;
  tab.sqlDirection ??= 'none';
  return tab;
}
function sqlLineButtons(tab, parsed) {
  const current = parsed.statements.findIndex(s => tab.sqlEnd >= s.scopeStart && tab.sqlEnd <= s.scopeEnd);
  return tab.sql.split('\n').map((_, index) => {
    const line = index + 1;
    const entries = parsed.statements.map((s, n) => ({ ...s, n })).filter(
      s => s.line === line,
    );
    const label = entries.length === 1
      ? t('runStatementLines', {
        number: entries[0].n + 1,
        start: line,
        end: entries[0].lastLine,
      })
      : t('multipleStatements', { count: entries.length });
    const action = entries.length === 1
      ? `sql-statement:${entries[0].n}`
      : `sql-line:${line}`;
    return `<div class="gutter-line ${
      entries.some(s => s.n === current) ? 'current' : ''
    }"><span class="gutter-number">${line}</span>${
      entries.length
        ? `<button class="gutter-run" data-action="${action}" title="${label}" aria-label="${label}">${
          icon('play')
        }</button>`
        : '<span class="gutter-gap"></span>'
    }</div>`;
  }).join('');
}
function queryPanel(tab) {
  queryTab();
  const resultId = `sql:${tab.id}`;
  const resultRows = orders.filter(row => row.status === 'paid');
  const resultPageRows = resultPageSlice(resultRows, resultId).filter(row =>
    Object.values(row).some(value => String(value).toLowerCase().includes(state.resultFilter.toLowerCase()))
  );
  return `<section class="sql-workbench ${
    tab.execution || tab.resultFixture ? 'has-sql-results' : ''
  }"><div class="toolbar"><button class="primary row" data-action="sql-all">${icon('play')} ${
    t('runAll')
  }</button><button class="button" data-action="sql-selection" disabled>${
    t('runSelection')
  }</button><button data-action="sql-current">${
    t('currentStatement')
  }</button><button class="button" data-action="sql-explain" title="${h(t('explainTarget'))}">${
    t('explainPlan')
  }</button>${ib('stop', t('cancelExecution'), 'cancel', 'disabled')}${ib('history', t('queryHistory'), 'history')}${
    ib('folder', t('queryFile'), 'query-file')
  }<span class="spacer"></span><span class="tag">${
    h(tab.database)
  }</span></div><div class="sql-editor-wrap"><div class="sql-gutter" aria-hidden="false"><div id="sql-gutter-lines"></div></div><div class="sql-input-surface"><pre id="sql-highlight" class="sql-highlight" aria-hidden="true"></pre><textarea id="sql-query" class="query-editor" wrap="off" spellcheck="false" aria-label="${
    t('sqlQuery')
  }" aria-describedby="sql-editor-note">${
    h(tab.sql)
  }</textarea></div></div><div class="sql-editor-info"><span id="sql-editor-note"></span><span class="spacer"></span><span id="sql-position"></span></div>${
    tab.execution
      ? `<section class="sql-results" aria-label="${t('executionPreview')}"><div class="sql-results-head"><strong>${
        h(
          tab.execution.labelKey
            ? t(tab.execution.labelKey, tab.execution.labelValues)
            : tab.execution.label,
        )
      }</strong><span class="spacer"></span><span class="tag warn">${t('notSent')}</span>${
        ib('x', t('closeResults'), 'sql-close-results')
      }</div><pre class="sql-execution-text">${
        highlightSql(tab.execution.sql)
      }</pre><div class="notice" style="margin:0 16px 12px">${t('exactSql')}</div></section>`
      : tab.resultFixture
      ? `<section class="sql-results" aria-label="${t('sqlResultSample')}"><div class="sql-results-head"><strong>${
        t('resultOne')
      }</strong><span class="muted">${t('readonlyResults')}</span><span class="spacer"></span><span class="tag">${
        t('sampleResult')
      }</span>${ib('columns', t('columnSettings'), 'result-columns')}${ib('download', t('exportData'), 'export')}${
        ib('x', t('closeResults'), 'sql-close-results')
      }</div><div class="toolbar"><input type="search" id="result-filter" aria-label="${
        h(t('filterPage'))
      }" placeholder="${h(t('filterPagePlaceholder'))}" value="${
        h(state.resultFilter)
      }"></div><div class="table-wrap">${
        simpleTable(
          ['id', 'order_no', 'status', 'total'].filter(c => !state.hiddenColumns['result']?.includes(c)),
          resultPageRows.map(row =>
            ['id', 'order_no', 'status', 'total'].filter(c => !state.hiddenColumns['result']?.includes(c)).map(c =>
              row[c]
            )
          ),
          true,
        )
      }</div>${resultPager(resultId, resultRows.length, { live: true, visible: resultPageRows.length })}</section>`
      : ''
  }</section>`;
}
function syncSqlEditor() {
  const input = $('#sql-query'), tab = queryTab();
  if (!input || !tab) return;
  const parsed = scanSql(tab.sql);
  $('#sql-highlight').innerHTML = highlightSql(tab.sql, parsed) + '\n';
  $('#sql-gutter-lines').innerHTML = sqlLineButtons(tab, parsed);
  $('#sql-editor-note').textContent = parsed.error
    || t('sqlRecognized', { count: parsed.statements.length });
  $('#sql-editor-note').classList.toggle('warn', Boolean(parsed.error));
  const before = tab.sql.slice(0, tab.sqlEnd);
  $('#sql-position').textContent = t('lineColumn', {
    line: before.split('\n').length,
    column: Array.from(before.split('\n').at(-1)).length + 1,
  });
  $('[data-action="sql-selection"]').disabled = !tab.sql.slice(
    tab.sqlStart,
    tab.sqlEnd,
  ).trim();
  $('[data-action="sql-current"]').disabled = !parsed.statements.some(s =>
    tab.sqlEnd >= s.scopeStart && tab.sqlEnd <= s.scopeEnd
  );
  $('[data-action="sql-all"]').disabled = !tab.sql.trim();
  const explain = $('[data-action="sql-explain"]');
  const target = explainTarget(tab, parsed);
  explain.disabled = !target;
  explain.title = target ? t('explainTarget') : t('explainLimit');
  syncSqlScroll();
}
function rememberSqlSelection() {
  const input = $('#sql-query'), tab = queryTab();
  if (!input || !tab || document.activeElement !== input) return;
  tab.sqlStart = input.selectionStart;
  tab.sqlEnd = input.selectionEnd;
  tab.sqlDirection = input.selectionDirection;
  syncSqlEditor();
}
function syncSqlScroll() {
  const input = $('#sql-query');
  if (!input) return;
  $('#sql-highlight').scrollTop = input.scrollTop;
  $('#sql-highlight').scrollLeft = input.scrollLeft;
  $('#sql-gutter-lines').style.transform = `translateY(${-input
    .scrollTop}px)`;
}
function explainTarget(tab, parsed = scanSql(tab.sql)) {
  const selected = tab.sql.slice(tab.sqlStart, tab.sqlEnd);
  if (selected.trim()) {
    return explainableSelection(selected)
      ? selected
      : '';
  }
  if (parsed.error) return '';
  const current = parsed.statements.find(s => tab.sqlEnd >= s.scopeStart && tab.sqlEnd <= s.scopeEnd);
  const sql = current ? tab.sql.slice(current.start, current.end) : '';
  return explainableSelection(sql) ? sql : '';
}
function requestSql(scope, index) {
  const tab = queryTab();
  if (!tab) return;
  const parsed = scanSql(tab.sql);
  let sql = '', labelKey = '', labelValues = {};
  if (scope === 'selection') {
    sql = tab.sql.slice(tab.sqlStart, tab.sqlEnd);
    labelKey = 'executeSelectedSql';
  } else if (scope === 'explain-selection' || scope === 'explain') {
    const selected = scope === 'explain-selection'
      ? tab.sql.slice(tab.sqlStart, tab.sqlEnd)
      : explainTarget(tab, parsed);
    if (!explainableSelection(selected)) {
      toast(t('explainLimit'));
      return;
    }
    sql = 'EXPLAIN FORMAT=JSON\n' + selected;
    labelKey = 'explainPlan';
  } else if (scope === 'all') {
    sql = tab.sql;
    labelKey = 'executeAllSql';
  } else {
    const statement = scope === 'current'
      ? parsed.statements.find(s => tab.sqlEnd >= s.scopeStart && tab.sqlEnd <= s.scopeEnd)
      : parsed.statements[index];
    if (!statement) {
      toast(parsed.error || t('noCurrentSql'));
      return;
    }
    sql = tab.sql.slice(statement.start, statement.end);
    labelKey = 'runStatementLines';
    labelValues = {
      number: parsed.statements.indexOf(statement) + 1,
      start: statement.line,
      end: statement.lastLine,
    };
  }
  if (!sql.trim()) {
    toast(t('noSelectedSql'));
    return;
  }
  tab.execution = { sql, labelKey, labelValues };
  tab.resultFixture = false;
  state.overlay = null;
  state.menu = null;
  render();
}
function sampleDdl() {
  const rows = databases[state.database][state.table],
    columns = Object.keys(rows[0]);
  const lines = columns.map(column =>
    `  \`${column}\` ${
      column === 'id'
        ? 'BIGINT NOT NULL AUTO_INCREMENT'
        : column === 'created_at'
        ? 'DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP'
        : ['total', 'revenue', 'price'].includes(column)
        ? 'DECIMAL(12,2) NOT NULL'
        : 'VARCHAR(255)' + (rows.some(row => row[column] === null)
          ? ' NULL'
          : ' NOT NULL')
    }`
  );
  if (columns.includes('id')) lines.push('  PRIMARY KEY (`id`)');
  return `CREATE TABLE \`${state.table}\` (\n${lines.join(',\n')}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;`;
}
function ddlPanel() {
  const sql = sampleDdl();
  return `<div class="toolbar"><span class="muted small">CREATE TABLE · ${
    h(state.table)
  }</span><span class="spacer"></span>${
    ib('copy', t('copyDdl'), 'copy-ddl')
  }</div><div class="sql-readonly"><div class="ddl-gutter" aria-hidden="true">${
    sql.split('\n').map((_, i) => i + 1).join('\n')
  }</div><pre class="ddl-code" aria-label="DDL SQL">${highlightSql(sql)}</pre></div><div class="sql-editor-info">${
    t('readonlyDdl')
  }<span class="spacer"></span>${t('catalogExample')}</div>`;
}

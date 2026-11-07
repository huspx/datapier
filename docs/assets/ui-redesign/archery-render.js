'use strict';
function renderArchery() {
  const i = ai.find(item => item.id === ap.instance),
    redis = i.provider === 'redis';
  const b = (zh, en, action, attrs = '') => ab(az(zh, en), action, attrs);
  let content;
  if (!ap.tabs.length && ap.view === 'query') {
    content = `<div class="empty empty-workspace"><strong>${
      h(t('emptyWorkspace'))
    }</strong><div class="empty-actions">${b('新建查询', 'New query', 'new-tab')}${
      state.compact
        ? `<button data-action="select-connection">${h(t('selectConnection'))}</button>`
        : ''
    }</div></div>`;
  } else if (ap.view === 'history') {
    content = modal(
      az('查询历史', 'Query history'),
      `<div class="history-layout">${historyPanel(true)}</div>`,
      `<span class="spacer"></span>${b('关闭', 'Close', 'query')} ${
        b(
          '打开为未执行草稿',
          'Open as unexecuted draft',
          'open',
          ahistory()[ap.page * ap.pageSize + ap.selectedHistory]
            && ap.task !== 'running'
            ? 'class="primary"'
            : 'disabled',
        )
      }`,
    );
  } else if (ap.view === 'schema' || ap.view === 'ddl') {
    content = `<div class="subnav"><button data-archery="schema" class="${ap.view === 'schema' ? 'active' : ''}">${
      t('structure')
    }</button><button data-archery="ddl" class="${
      ap.view === 'ddl' ? 'active' : ''
    }">DDL</button><span class="spacer"></span><span class="muted small">${
      h(ap.db)
    }.orders</span></div><div class="archery-body">${
      ap.view === 'ddl'
        ? '<pre class="archery-output">CREATE TABLE orders (\n  id BIGINT PRIMARY KEY,\n  status VARCHAR(32),\n  total DECIMAL(10,2)\n);</pre>'
        : `<div class="table-wrap">${
          simpleTable([t('field'), t('type')], [['id', 'BIGINT'], [
            'status',
            'VARCHAR(32)',
          ], ['total', 'DECIMAL(10,2)']])
        }</div>`
    }<p class="archery-note">${
      az(
        '平台返回的示例结构，只读。',
        'Example platform structure, read only.',
      )
    }</p></div>`;
  } else {
    const status = ap.task === 'running'
      ? az('任务运行中', 'Task running')
      : ap.task === 'unknown'
      ? az(
        '已结束等待；远端状态未知。',
        'Stopped waiting; remote state is unknown.',
      )
      : ap.task === 'denied'
      ? az(
        '权限拒绝，请联系平台管理员。',
        'Permission denied; contact the platform administrator.',
      )
      : ap.draft
      ? az('未执行草稿', 'Unexecuted draft')
      : '';
    const selectedCli = ap.cliEntries[ap.cliSelected ?? ap.cliEntries.length - 1];
    const cliValues = selectedCli?.values
      ?? (selectedCli ? [selectedCli.text] : []);
    const cliSize = selectedCli?.pageSize ?? 20;
    const cliPages = Math.max(1, Math.ceil(cliValues.length / cliSize));
    const cliPage = Math.min(selectedCli?.page ?? 0, cliPages - 1);
    const provenance =
      `<div class="archery-result-details" id="archery-result-details" popover="auto" role="dialog" aria-label="${
        az('结果说明', 'Result details')
      }"><div class="result-popup-head"><strong>${
        az('结果说明', 'Result details')
      }</strong><span class="spacer"></span>${`<button type="button" class="icon-button" data-result-info-close popovertarget="archery-result-details" popovertargetaction="hide" title="${
        h(az('关闭结果说明', 'Close result details'))
      }" aria-label="${h(az('关闭结果说明', 'Close result details'))}">${icon('x')}</button>`}</div><dl><dt>${
        az('来源', 'Source')
      }</dt><dd>Archery · ${h(redis && selectedCli ? selectedCli.instance : i.name)} · ${
        h(redis && selectedCli ? selectedCli.database : ap.db)
      }</dd>${
        ap.review
          ? `<dt>${az('操作', 'Operation')}</dt><dd>${
            az(
              '仅检查，不创建工单或执行 SQL',
              'Review only; no ticket or SQL execution',
            )
          }</dd>`
          : `<dt>${az('执行时上限', 'Executed limit')}</dt><dd>${
            redis && selectedCli ? selectedCli.limit : ap.executedLimit
          } ${az('行', 'rows')}</dd><dt>${az('结果范围', 'Coverage')}</dt><dd>${
            az(
              '仅包含平台本次返回的数据；平台未提供原查询总行数。SQL 的 LIMIT 和平台权限、配置可能进一步限制结果。',
              'Only the rows returned by the platform are included. The full query row count is not provided; SQL LIMIT, platform permissions and settings may further restrict results.',
            )
          }</dd><dt>${az('脱敏状态', 'Masking')}</dt><dd>${az('平台未提供', 'Not provided by the platform')}</dd>`
      }<dt>${az('数据说明', 'Data fidelity')}</dt><dd>${
        az(
          '平台返回值；原始类型与字节未提供，二进制或编码可能有损。复制和导出覆盖本次缓存。',
          'Platform values; original types and bytes are not provided. Binary or encoding may be lossy. Copy and export cover this cached response.',
        )
      }</dd></dl></div>`;
    const reviewStatus = ap.check === 'incomplete'
      ? az('检查未完成', 'Review incomplete')
      : ap.check === 'warning'
      ? az(
        '检查完成 · 警告 1 · 错误 0',
        'Review complete · warnings 1 · errors 0',
      )
      : ap.check === 'error'
      ? az(
        '检查完成 · 有错误 · 警告 0 · 错误 1',
        'Review complete · errors found · warnings 0 · errors 1',
      )
      : az(
        '检查完成 · 有错误 · 警告 1 · 错误 1',
        'Review complete · errors found · warnings 1 · errors 1',
      );
    const reviewRows = ap.check === 'incomplete' ? [] : [
      ...(ap.check === 'warning' || ap.check === 'complete'
        ? [['CHECK', '1', az('示例警告', 'Example warning')]]
        : []),
      ...(ap.check === 'error' || ap.check === 'complete'
        ? [[
          'CHECK',
          '2',
          az(
            '禁止无 WHERE 条件的 DELETE 语句',
            'DELETE without a WHERE clause is prohibited',
          ),
        ]]
        : []),
    ];
    const resultRows = ap.review ? reviewRows : ap.cachedRows;
    const resultPages = Math.max(
      1,
      Math.ceil(resultRows.length / ap.resultPageSize),
    );
    ap.resultPage = Math.min(ap.resultPage, resultPages - 1);
    const visibleRows = resultRows.slice(
      ap.resultPage * ap.resultPageSize,
      (ap.resultPage + 1) * ap.resultPageSize,
    );
    const grid = ap.review
      ? `<div class="toolbar"><span>${h(reviewStatus)}</span><span class="spacer"></span><label class="muted small">${
        az('预览检查状态', 'Preview review status')
      } <select data-review-preview aria-label="${az('预览检查状态', 'Preview review status')}">${
        [
          ['error', az('错误', 'Errors')],
          ['warning', az('警告', 'Warnings')],
          ['incomplete', az('未完成', 'Incomplete')],
          ['complete', az('完成', 'Complete')],
        ].map(([value, label]) => `<option value="${value}" ${ap.check === value ? 'selected' : ''}>${label}</option>`)
          .join('')
      }</select></label></div><div class="table-wrap">${
        simpleTable(['stage', 'errlevel', 'errormessage'], visibleRows)
      }${
        ap.check !== 'incomplete'
          ? b('单元格详情', 'Cell details', 'review-detail')
          : ''
      }${
        ap.reviewDetail
          ? `<div class="archery-group"><strong>errormessage</strong><p>${
            az(
              '禁止无 WHERE 条件的 DELETE 语句',
              'DELETE without a WHERE clause is prohibited',
            )
          }</p></div>`
          : ''
      }</div>`
      : `<div class="table-wrap">${simpleTable(['id', 'status', 'total'], visibleRows)}</div>`;
    if (redis) {
      content = `<section class="cli archery-cli"><div class="cli-head"><span class="cli-title">${
        icon('terminal')
      } Redis CLI</span><span class="muted small" title="${h(i.name)}">${az('当前输入', 'Current input')}: ${
        h(cliSummary(i.name))
      } / DB ${h(ap.db)}</span><span class="spacer"></span><label class="result-limit">${
        az('最多返回', 'Max rows')
      } <select data-request-limit aria-label="${az('命令请求上限', 'Command request limit')}" ${
        ap.task === 'running' ? 'disabled' : ''
      }>${
        [100, 500, 1000].map(n => `<option value="${n}" ${ap.requestLimit === n ? 'selected' : ''}>${n}</option>`).join(
          '',
        )
      }</select></label>${aib('history', t('commandHistory'), 'history')}${
        aib(
          'copy',
          az('复制执行记录', 'Copy execution log'),
          'copy-result',
          ap.cliEntries.length ? '' : 'disabled',
        )
      }${
        aib(
          'download',
          az('导出定位结果', 'Export located result'),
          'export-result',
          ap.cliEntries.length ? '' : 'disabled',
        )
      }${
        aib(
          'stop',
          az('结束等待', 'Stop waiting'),
          'stop',
          ap.task === 'running' ? '' : 'disabled',
        )
      }</div>${
        selectedCli
          ? `<div class="cli-head"><span class="muted small" title="${h(selectedCli.command)} · ${
            h(selectedCli.instance)
          }">${az('定位结果', 'Located result')}: ${h(cliSummary(selectedCli.command))} · ${
            h(cliSummary(selectedCli.instance))
          } / DB ${h(selectedCli.database)} · ${selectedCli.ms ?? 0} ms · ${cliValues.length} ${
            az('行', 'rows')
          }</span><span class="spacer"></span><button type="button" class="icon-button" data-result-info popovertarget="archery-result-details" aria-haspopup="dialog" aria-expanded="false" aria-controls="archery-result-details" title="${
            h(az('结果说明', 'Result details'))
          }" aria-label="${h(az('结果说明', 'Result details'))}">${
            icon('info')
          }</button></div>${provenance}<div class="table-footer result-pager"><span>${az('本页', 'Page')}: ${
            cliValues.slice(cliPage * cliSize, (cliPage + 1) * cliSize)
              .length
          } · ${az('已返回', 'Returned')}: ${cliValues.length}</span><div class="pager-controls"><label>${
            az('每页', 'Per page')
          } <select data-cli-page-size aria-label="${az('结果每页行数', 'Result page size')}">${
            resultPageSizes.map(n => `<option value="${n}" ${cliSize === n ? 'selected' : ''}>${n}</option>`).join('')
          }</select></label>${
            aib(
              'back',
              t('previousPage'),
              'cli-prev',
              cliPage > 0 ? '' : 'disabled',
            )
          }<span>${cliPage + 1} / ${cliPages}</span>${
            aib(
              'chev-r',
              t('nextPage'),
              'cli-next',
              cliPage + 1 < cliPages ? '' : 'disabled',
            )
          }</div></div>`
          : ''
      }<div class="cli-output" role="log" aria-label="${az('执行记录', 'Execution log')}"><div class="archery-note">${
        az(
          '平台文本结果受平台限制；原始类型和字节未知，编码可能有损。示例未发送请求。',
          'Platform text results are subject to platform limits; original types and bytes are unknown and encoding may be lossy. No request was sent in this preview.',
        )
      }</div>${status ? `<div class="muted">${h(status)}</div>` : ''}${
        cliExecutionLog(
          ap.cliEntries,
          ap.cliEntries.map((entry, index) =>
            index === (ap.cliSelected ?? ap.cliEntries.length - 1)
              ? {
                ...entry,
                text: cliValues.slice(
                  cliPage * cliSize,
                  (cliPage + 1) * cliSize,
                ).join('\n'),
              }
              : entry
          ),
        )
      }</div><div class="cli-prompt"><span class="prompt-label">DB ${
        h(ap.db)
      } ›</span><input data-af="command" value="${h(ap.command)}" aria-label="${h(t('redisCommand'))}" placeholder="${
        h(t('commandPlaceholder'))
      }" ${ap.task === 'running' ? 'disabled' : ''}>${
        aib(
          'play',
          t('runCommand'),
          'result',
          ap.task === 'running' ? 'disabled' : '',
        )
      }</div></section>`;
    } else {
      content = `<section class="sql-workbench archery-query-workbench ${
        ap.result ? 'has-sql-results' : ''
      }"><div class="toolbar">${
        ab(
          icon('play') + ' ' + t('runAll'),
          'result',
          `class="primary row" ${ap.task === 'running' ? 'disabled' : ''}`,
        )
      }${ab(t('runSelection'), 'selection', 'disabled')}${
        ab(
          t('currentStatement'),
          'result',
          ap.task === 'running' ? 'disabled' : '',
        )
      }${
        ab(
          t('explainPlan'),
          'explain',
          ap.task === 'running' ? 'disabled' : '',
        )
      }${
        aib(
          'stop',
          az('结束等待', 'Stop waiting'),
          'stop',
          ap.task === 'running' ? '' : 'disabled',
        )
      }<label class="result-limit" title="${
        az(
          '用于下次查询执行；平台仍可能限制返回行数。',
          'Applies to the next query execution; the platform may return fewer rows.',
        )
      }">${az('最多返回', 'Max rows')} <select data-request-limit aria-label="${
        az('查询请求上限', 'Query request limit')
      }" ${ap.task === 'running' ? 'disabled' : ''}>${
        [100, 500, 1000].map(n =>
          `<option value="${n}" ${ap.requestLimit === n ? 'selected' : ''}>${n} ${az('行', 'rows')}</option>`
        ).join('')
      }</select></label><span class="spacer"></span>${aib('folder', az('查询文件', 'Query file'), 'file')}${
        aib('history', t('queryHistory'), 'history')
      }${
        aib(
          'file-search',
          az('检查上线 SQL（不执行）', 'Review SQL (does not execute)'),
          'check',
          ap.task === 'running' ? 'disabled' : '',
        )
      }</div><div class="sql-editor-wrap"><div class="sql-gutter"><div class="gutter-line current"><span class="gutter-number">1</span>${
        ab(
          icon('play'),
          'result',
          `class="gutter-run" ${ap.task === 'running' ? 'disabled' : ''}`,
        )
      }</div></div><div class="sql-input-surface"><textarea class="query-editor archery-editor" data-af="sql" wrap="off" spellcheck="false" aria-label="${
        h(t('sqlQuery'))
      }">${h(ap.sql)}</textarea></div></div><div class="sql-editor-info"><span>${
        h(status)
      }</span><span class="spacer"></span><span>${
        az('示例预览 · 未发送请求', 'Example preview · no request sent')
      }</span></div>${
        ap.result
          ? `<section class="sql-results"><div class="sql-results-head"><strong>${
            ap.review ? az('SQL 检查', 'SQL review') : az('结果', 'Results')
          }</strong><span class="spacer"></span>${`<button type="button" class="icon-button" data-result-info popovertarget="archery-result-details" aria-haspopup="dialog" aria-expanded="false" aria-controls="archery-result-details" title="${
            h(az('结果说明', 'Result details'))
          }" aria-label="${h(az('结果说明', 'Result details'))}">${icon('info')}</button>`}${
            aib(
              'repeat',
              t('refreshData'),
              'result',
              ap.task === 'running' ? 'disabled' : '',
            )
          }${aib('copy', az('复制结果', 'Copy result'), 'copy-result')}${
            aib(
              'download',
              az('导出缓存结果', 'Export cached result'),
              'export-result',
            )
          }${
            aib('x', t('closeResults'), 'close-result')
          }</div>${provenance}${grid}<div class="table-footer result-pager"><span>${
            az(
              `本页 ${visibleRows.length} 行 · 已返回 ${resultRows.length} 行`,
              `Page: ${visibleRows.length} rows · Returned: ${resultRows.length} rows`,
            )
          }</span>${
            !ap.review && resultRows.length >= ap.executedLimit
              ? `<span class="warn small" title="${
                az(
                  '可调大上限后重新执行；平台仍可能进一步限制结果。',
                  'Increase the limit and run again; the platform may still restrict results.',
                )
              }">${az('已达上限', 'Limit reached')}</span>`
              : ''
          }<div class="pager-controls"><label class="page-size-control">${
            az('每页', 'Per page')
          } <select data-result-page-size aria-label="${az('结果每页行数', 'Result page size')}">${
            resultPageSizes.map(n => `<option value="${n}" ${ap.resultPageSize === n ? 'selected' : ''}>${n}</option>`)
              .join('')
          }</select></label>${
            aib(
              'back',
              t('previousPage'),
              'result-prev',
              ap.resultPage > 0 ? '' : 'disabled',
            )
          }<span class="mono">${
            az(
              `第 ${ap.resultPage + 1} / ${resultPages} 页`,
              `Page ${ap.resultPage + 1} / ${resultPages}`,
            )
          }</span>${
            aib(
              'chev-r',
              t('nextPage'),
              'result-next',
              ap.resultPage + 1 < resultPages ? '' : 'disabled',
            )
          }</div></div></section>`
          : ''
      }</section>`;
    }
  }
  $('#app').className = 'shell' + (state.compact ? ' compact' : '');
  const tabItems = ap.tabs.map(tab => ({
    ...tab,
    title: tab.query ? `${az('查询', 'Query')} ${tab.number}` : tab.title,
    database: tab.database,
  }));
  $('#app').innerHTML = `${sidebar()}<main class="main"><header class="context">${`<span class="connection-symbol">${
    marker(i.provider)
  }<img class="archery-source-badge" src="assets/archery-badge.svg?v=d6b5886d" alt="" aria-hidden="true"></span>`}<strong>${
    h(
      archeryConnections.find(c => c.server === activeServerId && c.instance === i.id)?.name || i.name,
    )
  }</strong>${
    redis
      ? `<select data-at aria-label="${az('数据库', 'Database')}">${
        (archeryConnections.find(c => c.server === activeServerId && c.instance === i.id)?.dbs || i.dbs).map(db =>
          `<option value="${h(db)}" ${db === ap.db ? 'selected' : ''}>DB ${h(db)}</option>`
        ).join('')
      }</select>`
      : `<span class="breadcrumb">/ ${h(ap.db)}</span>`
  }<span class="tag secondary">${i.provider.toUpperCase()}</span><span class="tag good">${
    az('只读', 'Read only')
  }</span></header><div class="workarea">${
    tabs(tabItems, ap.active, true)
  }${content}</div><footer class="statusbar"><span>${
    az('示例数据 · 未发送', 'Example data · not sent')
  }</span><span class="spacer"></span><span>${
    ap.task === 'unknown'
      ? az('远端状态未知', 'Remote state unknown')
      : ap.task === 'denied'
      ? az('权限拒绝', 'Permission denied')
      : ''
  }</span></footer></main>${overlay()}${archeryWizard()}${archeryDialogs()}${menu()}`;
  $('#preview-provider').value = i.provider;
  for (const option of $('#preview-provider').options) {
    option.disabled = false;
  }
  document.querySelector('.preview-note').textContent = az(
    '示例数据 · 未发送请求 · 不连接数据库',
    'Example data · no requests sent · no database connection',
  );
}

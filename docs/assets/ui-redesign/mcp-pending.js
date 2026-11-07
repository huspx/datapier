'use strict';
const mcpPreviewLabels = {
  'mcp-approval': ['MCP · 审批详情', 'MCP · Approval details'],
  'mcp-auth': ['MCP · 认证', 'MCP · Authentication'],
  'mcp-tasks': ['MCP · 任务结果', 'MCP · Task results'],
  'mcp-empty': ['MCP · 空状态', 'MCP · Empty'],
  'mcp-observer': ['MCP · 观察窗口', 'MCP · Observer window'],
  'mcp-unselected': ['MCP · 未选择', 'MCP · No selection'],
  'mcp-loading': ['MCP · 加载中', 'MCP · Loading'],
  'mcp-error': ['MCP · 加载失败', 'MCP · Loading failed'],
  'mcp-ended': ['MCP · 审批已结束', 'MCP · Approval ended'],
  'mcp-expired': ['MCP · 结果已过期', 'MCP · Result expired'],
};
const mcp = {
  scenario: 'mcp-approval',
  open: false,
  tab: 'approval',
  selected: 0,
  page: 0,
  size: 100,
  diagnostic: false,
  message: '',
  authHandoff: false,
};
const mz = (zh, en) => language === 'zh-CN' ? zh : en;
const mcpApprovals = [
  {
    title: ['执行命令', 'Execute command'],
    connection: 'Commerce production / commerce',
    expires: '2026-11-07 18:30 UTC',
    id: '7e91c151-13bc-4823-a197-1efb6f27c4a2',
    command: 'DELETE FROM commerce.orders\nWHERE created_at < ? AND region = ?;',
    parameters: ['2026-11-01', 'west'],
  },
  {
    title: ['执行命令', 'Execute command'],
    connection: 'Analytics reporting connection with a deliberately long name / reporting_warehouse',
    expires: '2026-11-07 19:00 UTC',
    id: '914321b2-13bc-4823-a197-1efb6f27c4a3',
    command: 'DELETE FROM reporting_warehouse.audit_events\nWHERE logged_at < ?;',
    parameters: ['2026-11-01'],
  },
];
const mcpTasks = [
  {
    title: ['执行命令', 'Execute command'],
    connection: 'Commerce production / commerce',
    status: ['已完成', 'Completed'],
    id: '548ac151-13bc-4823-a197-1efb6f27c4a2',
    created: '2026-11-07 17:00 UTC',
  },
  {
    title: ['执行命令', 'Execute command'],
    connection: 'Analytics reporting / reporting_warehouse',
    status: ['失败', 'Failed'],
    id: '648ac151-13bc-4823-a197-1efb6f27c4a2',
    created: '2026-11-07 17:05 UTC',
  },
];
const mcpRows = Array.from({ length: 65 }, (_, index) => [
  String(10421 + index),
  ['Ada', 'Lin', 'Sam', 'Alex'][index % 4],
  ['128.00', '74.50', '219.00', '36.00'][index % 4],
]);
function mcpSelectScenario(value) {
  mcp.scenario = value;
  mcp.tab = value === 'mcp-auth' ? 'auth' : ['mcp-tasks', 'mcp-expired'].includes(value) ? 'tasks' : 'approval';
  mcp.selected = value === 'mcp-unselected' ? -1 : 0;
  mcp.page = 0;
  mcp.diagnostic = false;
  mcp.message = '';
  mcp.authHandoff = false;
  mcp.open = true;
  state.overlay = null;
  serverForm = null;
  serverDelete = null;
  connectionDelete = null;
  ap.modal = false;
  authStage = null;
}
function mcpListRow(item, index, status, approval = false) {
  const selected = mcp.selected === index;
  const row = `<button class="mcp-list-row ${
    selected && !approval ? 'active' : ''
  }" data-mcp-action="select" data-index="${index}" aria-pressed="${selected}"><span class="mcp-row-text"><span class="mcp-row-title"><strong>${
    h(mz(...item.title))
  }</strong><span class="mcp-status">${h(status)}</span></span><span class="muted">${
    h(item.connection)
  }</span></span></button>`;
  if (!approval) return row;
  const disabled = !selected || ['mcp-observer', 'mcp-loading', 'mcp-error'].includes(mcp.scenario)
    || (mcp.scenario === 'mcp-ended' && index === 0);
  const actions = [['reject', 'x', mz('拒绝', 'Reject')], ['approve', 'check', mz('批准', 'Approve')]];
  return `<div class="mcp-approval-row ${selected ? 'active' : ''}">${row}<div class="mcp-row-actions">${
    actions.map(([action, glyph, label]) =>
      `<button class="icon-button" data-mcp-action="${action}" title="${h(label)}" aria-label="${h(label)}" ${
        disabled ? 'disabled' : ''
      }>${icon(glyph)}</button>`
    ).join('')
  }</div></div>`;
}
function mcpApproval() {
  const list = `<div class="mcp-list">${
    mcpApprovals.map((row, i) =>
      mcpListRow(
        row,
        i,
        mcp.scenario === 'mcp-ended' && i === 0 ? mz('已拒绝', 'Rejected') : mz('待审批', 'Pending approval'),
        true,
      )
    ).join('')
  }</div>`;
  if (mcp.scenario === 'mcp-loading') {
    return list + `<p class="mcp-empty">${mz('正在加载审批详情', 'Loading approval details')}</p>`;
  }
  if (mcp.scenario === 'mcp-error') {
    return `<p class="mcp-error" role="alert">${
      mz('审批详情加载失败，请重新选择重试。', 'Approval details failed to load. Select a request to retry.')
    }</p>` + list;
  }
  const item = mcpApprovals[mcp.selected];
  if (!item) return list + `<p class="mcp-empty">${mz('选择一项查看详情', 'Select an item to view details')}</p>`;
  const status = mcp.scenario === 'mcp-ended' && mcp.selected === 0
    ? mz('已拒绝', 'Rejected')
    : mz('待审批', 'Pending approval');
  return `${list}<section class="detail-section mcp-details"><div class="detail-summary"><strong>${
    h(mz(...item.title))
  }</strong><span class="spacer"></span><span>${status}</span></div>
    <dl class="facts">${fact(mz('连接 / 数据库', 'Connection / database'), item.connection)}${
    fact(mz('状态', 'Status'), status)
  }${fact(mz('到期时间', 'Expires'), item.expires)}</dl>
    ${
    mcp.scenario === 'mcp-observer'
      ? `<p class="muted">${
        mz('仅观察：请在控制窗口处理审批', 'Observe only: handle approvals in the controlling window')
      }</p>`
      : ''
  }
    <section class="detail-section"><h3>${mz('执行语句', 'Statement')}</h3><pre class="mcp-code mono">${
    h(item.command)
  }</pre></section>
    <section class="detail-section"><h3>${mz('绑定参数', 'Bindings')}</h3><div class="mcp-parameters">${
    item.parameters.map((value, index) =>
      `<div class="mcp-parameter"><span>${index + 1}</span><pre class="mono">${h(value)}</pre></div>`
    ).join('')
  }</div></section>
    <div class="mcp-tools"><button class="button" data-mcp-action="diagnostic" aria-expanded="${mcp.diagnostic}">${
    mz('完整诊断', 'Full diagnostics')
  }</button><button class="button" data-mcp-action="observe">${mz('查看执行', 'View execution')}</button></div>
    ${
    mcp.diagnostic
      ? `<pre class="mcp-code mono">${
        h(JSON.stringify({ request_id: item.id, command: item.command, parameters: item.parameters }, null, 2))
      }</pre>`
      : ''
  }</section>`;
}
function mcpAuth() {
  const observer = mcp.scenario === 'mcp-observer';
  const names = ['Commerce production', 'Analytics reporting connection with a deliberately long name'];
  return `<div class="mcp-list">${
    names.map(name =>
      `<div class="mcp-auth-row"><div class="mcp-row-text"><strong>${h(name)}</strong><span class="muted">${
        mz('待认证', 'Authentication required')
      }</span><span class="muted">${
        observer
          ? mz('请在控制窗口完成认证', 'Complete authentication in the controlling window')
          : mz('当前窗口可处理', 'This window can authenticate')
      }</span></div><button class="button" data-mcp-action="authenticate" ${observer ? 'disabled' : ''}>${
        mz('认证', 'Authenticate')
      }</button></div>`
    ).join('')
  }</div>`;
}
function mcpTask() {
  const item = mcpTasks[mcp.selected];
  const failed = mcp.selected === 1;
  const rows = mcpRows.slice(mcp.page * mcp.size, (mcp.page + 1) * mcp.size);
  return `<div class="mcp-tools"><button class="button" data-mcp-action="refresh">${
    mz('刷新任务', 'Refresh tasks')
  }</button></div><div class="mcp-list">${
    mcpTasks.map((row, i) => mcpListRow(row, i, mz(...row.status))).join('')
  }</div>
    <section class="detail-section mcp-details"><div class="detail-summary"><strong>${
    h(mz(...item.title))
  }</strong><span class="spacer"></span><span>${h(mz(...item.status))}</span></div><dl class="facts">${
    fact(mz('连接 / 数据库', 'Connection / database'), item.connection)
  }${fact(mz('创建时间', 'Created'), item.created)}</dl><button class="button" data-mcp-action="refresh">${
    mz('刷新任务', 'Refresh tasks')
  }</button>
    ${
    failed
      ? `<section class="detail-section"><h3>${
        mz('错误', 'Error')
      }</h3><div class="notice error-notice">Table 'reporting.customers' doesn't exist</div></section>`
      : `<div class="mcp-tools"><button class="button" data-mcp-action="result" aria-pressed="true">${
        mz('结果 1', 'Result 1')
      }</button></div>${
        mcp.scenario === 'mcp-expired'
          ? `<p class="mcp-error" role="alert">${
            mz('结果已过期或已释放', 'The result has expired or been released')
          }</p>`
          : `<p class="muted small">${mz('读取完成', 'Read complete')}</p><p class="muted small">${
            mz('仅显示当前已返回的数据', 'Only data already returned is shown')
          }</p><div class="mcp-table">${simpleTable(['order_id', 'customer_name', 'total_amount'], rows)}</div>
          <div class="result-pager mcp-result-pager"><label>${
            mz('每页', 'Per page')
          } <select data-mcp-size aria-label="${mz('每页行数', 'Rows per page')}">${
            [20, 50, 100, 200, 500, 1000].map(size =>
              `<option value="${size}" ${mcp.size === size ? 'selected' : ''}>${size}</option>`
            ).join('')
          }</select></label>
          <button class="icon-button" data-mcp-action="previous" title="${mz('上一页', 'Previous page')}" aria-label="${
            mz('上一页', 'Previous page')
          }" ${mcp.page === 0 ? 'disabled' : ''}>${icon('back')}</button><span>${
            mz(
              `第 ${mcp.page + 1} / ${Math.max(1, Math.ceil(mcpRows.length / mcp.size))} 页`,
              `Page ${mcp.page + 1} / ${Math.max(1, Math.ceil(mcpRows.length / mcp.size))}`,
            )
          }</span>
          <button class="icon-button" data-mcp-action="next" title="${mz('下一页', 'Next page')}" aria-label="${
            mz('下一页', 'Next page')
          }" ${(mcp.page + 1) * mcp.size >= mcpRows.length ? 'disabled' : ''}>${
            icon('chev-r')
          }</button><span class="muted">${mz(`共 ${mcpRows.length} 条`, `${mcpRows.length} entries`)}</span></div>`
      }`
  }</section>`;
}
function renderMcpPreview() {
  const status = document.querySelector('#app .statusbar');
  if (!status) return;
  const approvalCount = mcp.scenario === 'mcp-empty' ? 0 : mcp.scenario === 'mcp-ended' ? 1 : 2;
  const authCount = mcp.scenario === 'mcp-empty' ? 0 : 2;
  const count = approvalCount + authCount;
  status.insertAdjacentHTML(
    'beforeend',
    `<button data-mcp-action="open">${mz('MCP 待办', 'MCP pending')} (${count})</button>`,
  );
  if (!mcp.open) return;
  const empty = mcp.scenario === 'mcp-empty';
  const tabs = [['approval', `${mz('待审批', 'Approvals')} (${approvalCount})`], [
    'auth',
    `${mz('待认证', 'Authentication')} (${authCount})`,
  ], ['tasks', mz('Agent 任务', 'Agent tasks')]];
  const content = empty
    ? `<div class="mcp-empty">${
      mcp.tab === 'approval'
        ? mz('没有待审批项', 'No pending approvals')
        : mcp.tab === 'auth'
        ? mz('没有待认证连接', 'No connections awaiting authentication')
        : mz('没有 Agent 任务', 'No agent tasks')
    }</div>`
    : mcp.tab === 'approval'
    ? mcpApproval()
    : mcp.tab === 'auth'
    ? mcpAuth()
    : mcpTask();
  const body = mcp.authHandoff
    ? `<p>${
      mz(
        '认证交接预览。未发送请求，也未认证。',
        'Authentication handoff preview. No request was sent and no authentication was performed.',
      )
    }</p>`
    : `<p class="mcp-preview-note">${
      mz('示例数据 · 仅供预览 · 未发送', 'Sample data · preview only · not sent')
    }</p><nav class="mcp-tabs" role="tablist" aria-label="${mz('MCP 待办分区', 'MCP pending sections')}">${
      tabs.map(([id, label]) =>
        `<button role="tab" aria-selected="${mcp.tab === id}" class="${
          mcp.tab === id ? 'active' : ''
        }" data-mcp-action="tab" data-tab="${id}">${h(label)}</button>`
      ).join('')
    }</nav>${content}${mcp.message ? `<p class="mcp-message" role="status">${h(mz(...mcp.message))}</p>` : ''}`;
  const footer = `<span class="spacer"></span><button class="button" data-mcp-action="close">${
    mz('关闭', 'Close')
  }</button>`;
  const html = modal(
    mcp.authHandoff ? mz('认证预览', 'Authentication preview') : mz('MCP 待办', 'MCP pending'),
    body,
    footer,
  ).replace('class="overlay"', 'class="overlay mcp-overlay"').replace(
    'data-action="close-modal"',
    'data-mcp-action="close"',
  );
  document.querySelector('#app').insertAdjacentHTML('beforeend', html);
}
document.addEventListener('change', event => {
  if (event.target.id === 'preview-state') {
    if (event.target.value in mcpPreviewLabels) {
      event.stopImmediatePropagation();
      mcpSelectScenario(event.target.value);
      render();
    } else mcp.open = false;
  } else if (event.target.id === 'preview-source') mcp.open = false;
  else if (event.target.matches('[data-mcp-size]')) {
    mcp.size = Number(event.target.value);
    mcp.page = 0;
    render();
  }
}, true);
document.addEventListener('click', event => {
  const button = event.target.closest('[data-mcp-action]');
  if (!button || button.disabled) return;
  event.stopImmediatePropagation();
  const action = button.dataset.mcpAction;
  const focusAttributes = ['data-mcp-action', 'data-index', 'data-tab'];
  const focusSelector = focusAttributes.filter(name => button.hasAttribute(name))
    .map(name => `[${name}="${CSS.escape(button.getAttribute(name))}"]`).join('');
  if (action === 'open') {
    mcp.open = true;
    mcp.authHandoff = false;
    mcp.tab = 'approval';
    mcp.selected = 0;
    state.overlay = null;
    ap.modal = false;
    authStage = null;
    serverForm = null;
    serverDelete = null;
    connectionDelete = null;
  } else if (action === 'close') {
    mcp.open = false;
    mcp.authHandoff = false;
  } else if (action === 'tab') {
    mcp.tab = button.dataset.tab;
    mcp.selected = 0;
    mcp.page = 0;
    mcp.diagnostic = false;
    mcp.message = '';
  } else if (action === 'select') {
    mcp.selected = Number(button.dataset.index);
    mcp.page = 0;
    mcp.diagnostic = false;
    mcp.message = '';
    if (mcp.scenario === 'mcp-error') {
      mcp.scenario = 'mcp-approval';
      document.querySelector('#preview-state').value = 'mcp-approval';
      mcp.message = ['示例详情已重新加载 · 未发送请求。', 'Sample details reloaded · no request was sent.'];
    }
  } else if (action === 'authenticate') mcp.authHandoff = true;
  else if (action === 'diagnostic') mcp.diagnostic = !mcp.diagnostic;
  else if (action === 'observe') {
    mcp.tab = 'tasks';
    mcp.page = 0;
  } else if (action === 'result') {
    mcp.message = ['示例结果 · 未发送读取请求。', 'Sample result · no read request was sent.'];
  } else if (action === 'approve' || action === 'reject') {
    mcp.message = ['仅供预览，未发送审批决定。', 'Preview only; no approval decision was sent.'];
  } else if (action === 'refresh') {
    mcp.message = ['示例数据 · 未发送刷新请求。', 'Sample data · no refresh request was sent.'];
  } else if (action === 'previous') mcp.page--;
  else if (action === 'next') mcp.page++;
  render();
  (document.querySelector(focusSelector) || document.querySelector('[data-mcp-action="close"]'))?.focus({
    preventScroll: true,
  });
}, true);
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && mcp.open) {
    event.stopImmediatePropagation();
    event.preventDefault();
    mcp.open = false;
    mcp.authHandoff = false;
    render();
    document.querySelector('[data-mcp-action="open"]')?.focus({ preventScroll: true });
  }
}, true);

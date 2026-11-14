'use strict';

const samplePreviewState = {
  kind: '',
  phase: 'unread',
  message: '',
  draft: 'SELECT id, name FROM customers\nWHERE active = 1;',
  confirm: false,
};
const samplePreviewBaseline = 'SELECT id, name FROM customers;';
const sampleText = (zh, en) => language === 'zh-CN' ? zh : en;
const sampleKinds = ['sample-cli-value', 'sample-paste', 'sample-unsaved', 'sample-history-error'];

function samplePreview(kind) {
  const s = samplePreviewState;
  if (s.kind !== kind) {
    s.kind = kind;
    s.phase = kind === 'sample-history-error' ? 'error' : 'unread';
    s.message = '';
    s.draft = 'SELECT id, name FROM customers\nWHERE active = 1;';
    s.confirm = false;
  }
  const note = `<p class="muted small">${
    sampleText('示例数据 · 仅供预览 · 未发送任何请求', 'Sample data · preview only · no requests sent')
  }</p>`;
  const close = `<span class="spacer"></span><button class="button" data-sample-action="close">${
    sampleText('关闭', 'Close')
  }</button>`;
  const message = s.message ? `<p role="status" class="muted">${h(s.message)}</p>` : '';
  if (kind === 'sample-unsaved') {
    if (s.confirm) {
      return modal(
        sampleText('未确认输入', 'Unconfirmed input'),
        `${note}<p>${
          sampleText(
            '关闭将丢弃本页示例中的未确认输入。',
            'Closing will discard the unconfirmed input in this page example.',
          )
        }</p>`,
        `<span class="spacer"></span><button class="button" data-sample-action="continue">${
          sampleText('继续编辑', 'Continue editing')
        }</button><button class="button" data-sample-action="discard">${
          sampleText('丢弃并关闭', 'Discard and close')
        }</button>`,
      )
        .replace('data-action="close-modal"', 'data-sample-action="continue"');
    }
    return modal(
      sampleText('编辑输入', 'Edit input'),
      `${note}<p>${
        sampleText('未确认输入 · 关闭前需要处理', 'Unconfirmed input · resolve before closing')
      }</p><label for="sample-unsaved-input">${
        sampleText('SQL 输入', 'SQL input')
      }</label><textarea id="sample-unsaved-input" rows="6">${h(s.draft)}</textarea>${message}`,
      close,
    )
      .replace('data-action="close-modal"', 'data-sample-action="close"');
  }
  if (kind === 'sample-paste') {
    return modal(
      sampleText('粘贴预览', 'Paste preview'),
      `${note}<p>${
        sampleText('目标：示例 customers 表，name 列，id = 42', 'Target: sample customers table, name column, id = 42')
      }</p><div class="detail-section"><h3>${
        sampleText('原值', 'Original value')
      }</h3><pre class="preview-code">Alex</pre></div><div class="detail-section"><h3>${
        sampleText('新值', 'New value')
      }</h3><pre class="preview-code">Morgan</pre></div><p class="muted">${
        sampleText('仅展示对比，不会应用粘贴。', 'Comparison only; paste will not be applied.')
      }</p>`,
      `${close}<button class="primary" disabled>${sampleText('确认粘贴', 'Confirm paste')}</button>`,
    )
      .replace('data-action="close-modal"', 'data-sample-action="close"');
  }
  const history = kind === 'sample-history-error';
  const phaseControls = `<div class="mcp-tools" aria-label="${sampleText('切换示例状态', 'Switch sample state')}">${
    ['unread', 'loading', 'error'].map(phase =>
      `<button class="button" data-sample-action="phase" data-sample-phase="${phase}" aria-pressed="${
        s.phase === phase
      }">${
        phase === 'unread'
          ? sampleText('未读取', 'Not loaded')
          : phase === 'loading'
          ? sampleText('加载中', 'Loading')
          : sampleText('失败', 'Failed')
      }</button>`
    ).join('')
  }</div>`;
  const status = s.phase === 'loading'
    ? `<p role="status">${
      history
        ? sampleText('正在加载完整文本…', 'Loading full text…')
        : sampleText('正在读取完整值…', 'Loading full value…')
    }</p>`
    : s.phase === 'error'
    ? `<div role="alert"><p class="mcp-error">${
      history
        ? sampleText(
          '示例：完整文本加载失败，保留历史摘要。',
          'Example: full text loading failed; the history summary is retained.',
        )
        : sampleText('示例：完整值读取失败。', 'Example: full value loading failed.')
    }</p><button class="button" data-sample-action="retry">${sampleText('重试', 'Retry')}</button></div>`
    : `<p>${
      history
        ? sampleText('尚未读取完整文本。', 'Full text has not been loaded.')
        : sampleText('尚未读取完整值。', 'Full value has not been loaded.')
    }</p><button class="button" data-sample-action="retry">${sampleText('读取完整内容', 'Load full content')}</button>`;
  const context = history
    ? `<div class="detail-section"><h3>${
      sampleText('历史摘要', 'History summary')
    }</h3><p><code>SELECT id, name FROM customers…</code></p><p class="muted">${
      sampleText('示例记录 · 本地历史', 'Sample entry · local history')
    }</p></div>`
    : `<div class="detail-section"><h3>${
      sampleText('值摘要', 'Value summary')
    }</h3><p><code>customers.profile</code> · ${sampleText('大字段', 'Large value')}</p></div>`;
  return modal(
    history ? sampleText('历史全文', 'History full text') : sampleText('CLI 完整值', 'CLI full value'),
    `${note}${context}<p class="muted small">${
      sampleText('以下按钮切换示例状态，不执行读取。', 'The controls below switch sample states and do not read data.')
    }</p>${phaseControls}${status}${message}`,
    close,
  )
    .replace('data-action="close-modal"', 'data-sample-action="close"');
}

document.addEventListener('input', event => {
  if (state.overlay === 'sample-unsaved' && event.target.id === 'sample-unsaved-input') {
    samplePreviewState.draft = event.target.value;
  }
}, true);

document.addEventListener('change', event => {
  if (event.target.id === 'preview-state') samplePreviewState.kind = '';
}, true);

function sampleRequestClose() {
  if (state.overlay === 'sample-unsaved' && samplePreviewState.draft !== samplePreviewBaseline) {
    samplePreviewState.confirm = true;
    render();
    document.querySelector('[data-sample-action="continue"]')?.focus();
    return;
  }
  state.overlay = null;
  samplePreviewState.kind = '';
  render();
  document.querySelector('#preview-state')?.focus();
}

document.addEventListener('click', event => {
  const button = event.target.closest('[data-sample-action]');
  if (!button || !sampleKinds.includes(state.overlay)) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  const action = button.dataset.sampleAction;
  if (action === 'close') {
    sampleRequestClose();
    return;
  }
  if (action === 'discard') {
    state.overlay = null;
    samplePreviewState.kind = '';
    render();
    document.querySelector('#preview-state')?.focus();
    return;
  }
  if (action === 'continue') samplePreviewState.confirm = false;
  else if (action === 'phase') {
    samplePreviewState.phase = button.dataset.samplePhase;
    samplePreviewState.message = '';
  } else if (action === 'retry') {
    samplePreviewState.message = sampleText(
      '仅供预览，未发送读取请求；状态保持不变。',
      'Preview only; no read request was sent and the state is unchanged.',
    );
  }
  render();
  const selector = action === 'continue'
    ? '#sample-unsaved-input'
    : action === 'phase'
    ? `[data-sample-action="phase"][data-sample-phase="${button.dataset.samplePhase}"]`
    : '[data-sample-action="retry"]';
  document.querySelector(selector)?.focus();
}, true);

document.addEventListener('keydown', event => {
  if (event.key !== 'Escape' || !sampleKinds.includes(state.overlay) || document.querySelector(':popover-open')) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  if (samplePreviewState.confirm) {
    samplePreviewState.confirm = false;
    render();
    document.querySelector('#sample-unsaved-input')?.focus();
  } else sampleRequestClose();
}, true);

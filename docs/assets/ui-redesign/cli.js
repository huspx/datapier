'use strict';
const cliScrollPositions = new WeakMap();
function appendCliRecord(entries, entry) {
  entries.push(entry);
  if (entries === state.cliEntries) state.cliSelected = entries.length - 1;
  cliScrollPositions.delete(entries);
}
function cliExecutionLog(entries, displayEntries = entries) {
  // HTML 重绘会替换输出节点；按记录集合恢复滚动，新执行则显示末尾。
  requestAnimationFrame(() => {
    const current = previewSource === 'archery'
      ? ap.cliEntries
      : state.cliEntries;
    const output = document.querySelector('.cli-output');
    if (!output || current !== entries) return;
    output.scrollTop = cliScrollPositions.get(entries)
      ?? output.scrollHeight;
  });
  return displayEntries.map((entry, index) =>
    `<div class="cli-entry" data-cli-entry ${
      previewSource === 'archery'
        ? `data-archery="cli-locate:${index}" role="button" tabindex="0" aria-label="${
          h(az('定位结果', 'Locate result'))
        }: ${h(entry.command)}"`
        : `data-cli-locate="${index}" role="button" tabindex="0" aria-label="${h(az('定位结果', 'Locate result'))}: ${
          h(entry.command)
        }"`
    }><div class="command-line"><span class="muted">›</span><span class="cli-entry-command">${
      h(entry.command)
    }</span><span class="muted small">${h(entry.instance)} · DB ${h(entry.database)}</span><span class="tag ${
      entry.status === 'error'
        ? 'danger'
        : entry.status === 'running' || entry.status === 'unknown'
        ? 'warn'
        : 'good'
    }">${
      entry.status === 'running'
        ? t('running')
        : entry.status === 'error'
        ? t('failure')
        : entry.status === 'unknown'
        ? az('等待已结束', 'Waiting stopped')
        : t('completedMs', { ms: entry.ms ?? 1 })
    }</span></div>${
      entry.rows
        ? simpleTable([t('field'), t('value')], entry.rows)
        : `<pre class="${entry.status === 'error' ? 'danger' : ''}">${h(entry.text || '')}</pre>`
    }</div>`
  ).join('');
}
document.addEventListener('scroll', event => {
  if (!event.target.matches?.('.cli-output')) return;
  const entries = previewSource === 'archery'
    ? ap.cliEntries
    : state.cliEntries;
  cliScrollPositions.set(entries, event.target.scrollTop);
}, true);
function directCliBounds(panel) {
  const controls = [...panel.querySelectorAll('.cli-head')].reduce(
    (height, head) => height + head.offsetHeight,
    0,
  )
    + panel.querySelector('.cli-prompt').offsetHeight
    + (panel.querySelector('.cli-history')?.offsetHeight || 0) + 12;
  const min = Math.ceil(controls + 40);
  // 标签栏与 CLI 同属 workarea，需先扣除其固定高度。
  const available = panel.parentElement.clientHeight
    - (panel.parentElement.querySelector(':scope > .tabs')?.offsetHeight
      || 0);
  return {
    min,
    max: Math.max(min, Math.floor(available - 128)),
    available: Math.max(1, available),
  };
}
function updateDirectCliHeight(
  preferred = state.cliShare,
  remember = false,
) {
  const panel = document.querySelector('[data-resizable-cli]');
  if (!panel) return;
  const { min, max, available } = directCliBounds(panel);
  const share = Math.max(0.15, Math.min(0.85, preferred));
  const height = Math.round(
    Math.max(min, Math.min(max, available * share)),
  );
  const output = panel.querySelector('.cli-output');
  const following = output.scrollTop + output.clientHeight >= output.scrollHeight - 2;
  panel.style.height = `${height}px`;
  if (remember) state.cliShare = share;
  const handle = panel.querySelector('[data-cli-resizer]');
  handle.setAttribute('aria-valuemin', 15);
  handle.setAttribute('aria-valuemax', 85);
  handle.setAttribute('aria-valuenow', Math.round(share * 100));
  handle.setAttribute('aria-valuetext', `${Math.round(share * 100)}%`);
  if (following) output.scrollTop = output.scrollHeight;
}
let cliHeightDrag = null;
document.addEventListener('pointerdown', event => {
  const handle = event.target.closest('[data-cli-resizer]');
  if (!handle || event.button !== 0) return;
  event.preventDefault();
  handle.focus();
  handle.setPointerCapture(event.pointerId);
  cliHeightDrag = {
    handle,
    pointer: event.pointerId,
    startY: event.clientY,
    height: handle.parentElement.getBoundingClientRect().height,
  };
  document.body.classList.add('cli-resizing');
});
document.addEventListener('pointermove', event => {
  if (!cliHeightDrag || cliHeightDrag.pointer !== event.pointerId) return;
  updateDirectCliHeight(
    (cliHeightDrag.height + cliHeightDrag.startY - event.clientY)
      / directCliBounds(cliHeightDrag.handle.parentElement).available,
    true,
  );
});
function finishCliHeightDrag(event) {
  if (!cliHeightDrag || cliHeightDrag.pointer !== event.pointerId) return;
  if (cliHeightDrag.handle.hasPointerCapture(event.pointerId)) {
    cliHeightDrag.handle.releasePointerCapture(event.pointerId);
  }
  cliHeightDrag = null;
  document.body.classList.remove('cli-resizing');
}
document.addEventListener('pointerup', finishCliHeightDrag);
document.addEventListener('pointercancel', finishCliHeightDrag);
document.addEventListener('keydown', event => {
  const handle = event.target.closest('[data-cli-resizer]');
  if (
    !handle || !['ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)
  ) return;
  event.preventDefault();
  updateDirectCliHeight(
    event.key === 'Home'
      ? 0.15
      : event.key === 'End'
      ? 0.85
      : state.cliShare + (event.key === 'ArrowUp' ? 0.05 : -0.05),
    true,
  );
});
// 窗口缩放按比例重算实际高度，最小可用空间不覆盖比例偏好。
window.addEventListener(
  'resize',
  () => requestAnimationFrame(() => updateDirectCliHeight()),
);
function cliSummary(value) {
  const line = String(value).split(/\r?\n/, 1)[0];
  const chars = Array.from(line);
  return chars.length > 32 ? chars.slice(0, 32).join('') + '…' : line;
}
function cli() {
  const running = state.cliState === 'running';
  const located = state.cliEntries[state.cliSelected ?? state.cliEntries.length - 1];
  requestAnimationFrame(() => updateDirectCliHeight());
  return `<section class="cli" data-resizable-cli><div class="cli-resize-handle" data-cli-resizer role="separator" aria-orientation="horizontal" tabindex="0" title="${
    az(
      '拖动调整 CLI 高度；方向键也可调整',
      'Drag to resize CLI; arrow keys also adjust height',
    )
  }" aria-label="${
    az('调整 Redis CLI 高度', 'Resize Redis CLI height')
  }"></div><div class="cli-head"><span class="cli-title">${
    icon('terminal')
  } Redis CLI</span><span class="muted small">${az('当前输入', 'Current input')}: Cache / DB ${
    state.cliDb ?? az('未知', 'Unknown')
  }</span><span class="spacer"></span>${ib('history', t('commandHistory'), 'cli-history')}${
    ib(
      'stop',
      t('cancelExecution'),
      'cli-cancel',
      running ? '' : 'disabled',
    )
  }${ib('chev', t('collapseCli'), 'cli-toggle')}</div>${
    located
      ? `<div class="cli-head"><span class="muted small" title="${h(located.command)} · ${h(located.instance)}">${
        az('定位结果', 'Located result')
      }: ${h(cliSummary(located.command))} · ${h(cliSummary(located.instance))} / DB ${h(located.database)} · ${
        located.ms ?? 0
      } ms</span></div>`
      : ''
  }${
    state.cliHistory
      ? `<div class="cli-history"><span class="muted small">${t('recent')}</span>${
        [...state.cliEntries].reverse().map(entry =>
          `<button data-command="${h(entry.command)}">${h(entry.command)}</button>`
        ).join('')
      }</div>`
      : ''
  }<div class="cli-output" role="log" aria-label="${az('执行记录', 'Execution log')}">${
    cliExecutionLog(state.cliEntries)
  }</div><div class="cli-prompt"><span class="prompt-label">localhost:6379[${
    state.cliDb ?? '?'
  }] ›</span><input id="cli-command" aria-label="${t('redisCommand')}" placeholder="${
    t('commandPlaceholder')
  }" value="${h(state.cliText)}" ${running ? 'disabled' : ''}>${
    ib('play', t('runCommand'), 'cli-run', running ? 'disabled' : '')
  }</div></section>`;
}

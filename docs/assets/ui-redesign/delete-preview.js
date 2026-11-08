'use strict';

// 这些控件仅展示状态，不发起或取消删除。
function deleteStateExamples() {
  return `<section class="detail-section" data-delete-state-example data-stage="pending">
    <strong>${az('删除状态交互示例（未发送请求）', 'Deletion state example (no request sent)')}</strong>
    <p data-delete-state-message role="status" aria-live="polite">${
    az(
      '待确认：目标已冻结，可核对后确认。',
      'Awaiting confirmation: the target is frozen and can be reviewed before confirming.',
    )
  }</p>
    <div class="row">
      <button type="button" class="button" data-delete-example="running">${
    az('模拟删除中', 'Simulate deleting')
  }</button>
      <button type="button" class="button" data-delete-example="failed" disabled>${
    az('模拟失败', 'Simulate failure')
  }</button>
      <button type="button" class="button" data-delete-example="unknown" disabled>${
    az('停止等待', 'Stop waiting')
  }</button>
      <button type="button" class="button" data-delete-example="pending">${az('重置示例', 'Reset example')}</button>
    </div>
    <p class="muted small" data-delete-confirm-reason></p>
  </section>`;
}

document.addEventListener('click', event => {
  const button = event.target.closest('[data-delete-example]');
  if (!button || button.disabled) return;
  const section = button.closest('[data-delete-state-example]');
  const modalElement = section.closest('.modal');
  if (!modalElement) return;
  event.preventDefault();
  event.stopPropagation();
  const stage = button.dataset.deleteExample;
  section.dataset.stage = stage;
  const messages = {
    pending: az(
      '待确认：目标已冻结，可核对后确认。',
      'Awaiting confirmation: the target is frozen and can be reviewed before confirming.',
    ),
    running: az(
      '删除中（模拟）：请求在途；停止等待不会取消远端操作。',
      'Deleting (simulation): the request is in progress; stopping the wait does not cancel remote work.',
    ),
    failed: az(
      '删除失败（模拟）：目标和错误保留，请检查原因后重试。',
      'Deletion failed (simulation): the target and error remain available; inspect the cause before retrying.',
    ),
    unknown: az(
      '已停止等待（模拟）：远端结果未知，请先核对目标状态，勿重复删除。',
      'Stopped waiting (simulation): the remote outcome is unknown; check the target before attempting deletion again.',
    ),
  };
  section.querySelector('[data-delete-state-message]').textContent = messages[stage];
  section.querySelector('[data-delete-example="running"]').disabled = stage === 'running' || stage === 'unknown';
  section.querySelector('[data-delete-example="failed"]').disabled = stage !== 'running';
  section.querySelector('[data-delete-example="unknown"]').disabled = stage !== 'running';
  const confirm = modalElement.querySelector(
    '[data-action="confirm-delete-connection"], [data-action="confirm-delete-resource"]',
  );
  const reason = section.querySelector('[data-delete-confirm-reason]');
  const blocked = stage === 'running' || stage === 'unknown';
  if (confirm) {
    confirm.disabled = blocked;
    confirm.setAttribute('aria-disabled', String(blocked));
  }
  reason.textContent = stage === 'running'
    ? az('请求在途，暂不能再次确认删除。', 'Deletion cannot be confirmed again while the request is in progress.')
    : stage === 'unknown'
    ? az(
      '结果未知，请先核对目标；重置示例仅切换示例状态。',
      'The outcome is unknown; check the target first. Resetting only changes the example state.',
    )
    : '';
});

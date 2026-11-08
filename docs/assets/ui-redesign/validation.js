'use strict';
const $ = selector => document.querySelector(selector);
const h = value =>
  String(value).replace(
    /[&<>"']/g,
    c => ({
      '&': '&amp;',
      '<': '&lt;',
      '>': '&gt;',
      '"': '&quot;',
      "'": '&#39;',
    }[c]),
  );
function fieldError(field, message) {
  field.setAttribute('aria-invalid', 'true');
  let error = field.parentElement.querySelector('.field-error');
  if (!error) {
    error = document.createElement('span');
    error.className = 'field-error';
    error.setAttribute('role', 'alert');
    field.parentElement.append(error);
  }
  error.textContent = message;
  field.closest('details')?.setAttribute('open', '');
  if (field.closest('[hidden]')) field.closest('[hidden]').hidden = false;
  field.focus();
}
function clearFieldErrors() {
  document.querySelectorAll('.field-error[role="alert"]').forEach(error => error.remove());
  document.querySelectorAll('[aria-invalid="true"]').forEach(field => field.removeAttribute('aria-invalid'));
}
function syncConditionalRequirements() {
  const setRequired = (field, required) => {
    if (!field) return;
    field.required = required;
    field.setAttribute('aria-required', String(required));
    const label = field.closest('label');
    let marker = label.querySelector('[data-conditional-required]');
    if (!marker) {
      marker = document.createElement('span');
      marker.dataset.conditionalRequired = '';
      marker.className = 'required-mark';
      marker.setAttribute('aria-hidden', 'true');
      marker.textContent = ' *';
      label.insertBefore(marker, field);
    }
    marker.hidden = !required;
  };
  const certificate = document.querySelector(
    '[data-connection-field="cert"]',
  );
  const key = document.querySelector('[data-connection-field="key"]');
  if (certificate && key) {
    const tls = document.querySelector('[data-connection-check="tls"]')
      ?.checked;
    setRequired(certificate, !!tls && !!key.value.trim());
    setRequired(key, !!tls && !!certificate.value.trim());
  }
  const sort = document.getElementById('sort-column');
  if (sort) {
    setRequired(document.getElementById('sort-direction'), !!sort.value);
  }
  const ttlPolicy = document.getElementById('redis-ttl-policy');
  if (ttlPolicy) {
    const setting = ttlPolicy.value === 'set';
    document.getElementById('ttl-value-label').hidden = !setting;
    setRequired(document.getElementById('redis-ttl-value'), setting);
  }
  const filterColumn = document.getElementById('filter-column');
  if (filterColumn) {
    const operator = document.getElementById('filter-operator');
    const value = document.getElementById('filter-value');
    operator.disabled = !filterColumn.value;
    value.disabled = !filterColumn.value || ['is_null', 'is_not_null'].includes(operator.value);
  }
  document.querySelectorAll('[data-new-column]').forEach(row => {
    row.querySelector('[data-new-value]').disabled = row.querySelector('[data-new-mode]').value !== 'value';
  });
  document.querySelectorAll('label').forEach(label => {
    const field = [...label.children].find(child =>
      child.matches(
        'input:not([type="checkbox"]):not([type="radio"]), select, textarea',
      )
    );
    if (!field || label.querySelector('.field-caption')) return;
    const preceding = [...label.childNodes].slice(
      0,
      [...label.childNodes].indexOf(field),
    );
    if (
      !preceding.some(node =>
        node.nodeType === Node.ELEMENT_NODE
        && (node.matches(
          '.required-mark, [style*="--danger"], #ttl-required',
        ))
      )
    ) return;
    const caption = document.createElement('span');
    caption.className = 'field-caption';
    label.insertBefore(caption, preceding[0]);
    preceding.forEach(node => caption.append(node));
  });
}
document.addEventListener(
  'input',
  () => queueMicrotask(syncConditionalRequirements),
  true,
);
document.addEventListener(
  'change',
  () => queueMicrotask(syncConditionalRequirements),
  true,
);

function validateRequiredFields(root) {
  for (const field of root.querySelectorAll('[required]')) {
    if (field.disabled || field.readOnly) continue;
    if (!field.checkValidity()) {
      fieldError(
        field,
        field.validity.valueMissing
          ? (language === 'zh-CN'
            ? '此项不能为空。'
            : 'This field is required.')
          : (language === 'zh-CN'
            ? '请输入有效的字段值。'
            : 'Enter a valid value.'),
      );
      return false;
    }
  }
  return true;
}
function validateAdvancedJson(root) {
  for (const field of root.querySelectorAll('#filter-advanced-json, #sort-advanced-json, #new-row-json')) {
    if (!field.value.trim()) continue;
    const array = field.id !== 'new-row-json';
    try {
      const value = JSON.parse(field.value);
      if (array ? Array.isArray(value) : value && typeof value === 'object' && !Array.isArray(value)) continue;
    } catch {}
    fieldError(
      field,
      array
        ? (language === 'zh-CN' ? '请输入有效的 JSON 数组。' : 'Enter a valid JSON array.')
        : (language === 'zh-CN' ? '请输入有效的 JSON 对象。' : 'Enter a valid JSON object.'),
    );
    return false;
  }
  return true;
}
document.addEventListener('click', event => {
  const button = event.target.closest(
    '.modal button.primary, .modal button[data-action^="request-"]',
  );
  if (
    button && !button.disabled
    && (!validateRequiredFields(button.closest('.modal')) || !validateAdvancedJson(button.closest('.modal')))
  ) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }
}, true);
function validateConnection() {
  clearFieldErrors();
  const form = state.connectionForm;
  const required = language === 'zh-CN'
    ? '此项不能为空。'
    : 'This field is required.';
  for (const name of ['name', 'host']) {
    if (!form[name].trim()) {
      fieldError(
        document.querySelector(`[data-connection-field="${name}"]`),
        required,
      );
      return false;
    }
  }
  if (
    !/^\d+$/.test(form.port) || Number(form.port) < 1
    || Number(form.port) > 65535
  ) {
    fieldError(
      document.querySelector('[data-connection-field="port"]'),
      language === 'zh-CN'
        ? '请输入 1–65535 的端口。'
        : 'Enter a port from 1 to 65535.',
    );
    return false;
  }
  if (
    (!form.tls && (form.ca || form.cert || form.key))
    || Boolean(form.cert) !== Boolean(form.key)
  ) {
    const field = document.querySelector(
      `[data-connection-field="${
        !form.tls
          ? (form.ca ? 'ca' : form.cert ? 'cert' : 'key')
          : form.cert
          ? 'key'
          : 'cert'
      }"]`,
    );
    fieldError(
      field,
      language === 'zh-CN'
        ? '启用 TLS 并成对提供证书和密钥。'
        : 'Enable TLS and provide both certificate and key.',
    );
    return false;
  }
  return true;
}
function validateTransfer(path) {
  clearFieldErrors();
  if (!path.value.trim()) {
    fieldError(
      path,
      language === 'zh-CN' ? '请输入文件路径。' : 'Enter a file path.',
    );
    return false;
  }
  for (
    const [id, valid] of [[
      'transfer-mapping',
      value => value && typeof value === 'object' && !Array.isArray(value),
    ], [
      'transfer-binary',
      value =>
        Array.isArray(value)
        && value.every(item => typeof item === 'string'),
    ]]
  ) {
    const field = document.getElementById(id);
    if (!field) continue;
    try {
      if (valid(JSON.parse(field.value))) continue;
    } catch {}
    fieldError(
      field,
      language === 'zh-CN'
        ? '请输入有效的 JSON 配置。'
        : 'Enter valid JSON configuration.',
    );
    return false;
  }
  return true;
}

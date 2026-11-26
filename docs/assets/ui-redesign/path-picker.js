'use strict';
// 示例目录与文件只存在于本页面，不读取本机文件系统。
(() => {
  const tree = {
    '/': [['repo', 'dir'], ['home', 'dir']],
    '/repo': [['app', 'dir']],
    '/repo/app': [
      ['sql', 'dir'],
      ['data', 'dir'],
      ['证书', 'dir'],
      ['空目录', 'dir'],
      ['受限目录', 'error'],
      ['orders.sql', 'file'],
      ['project.zip', 'file'],
    ],
    '/repo/app/sql': [['check.sql', 'file'], ['report.sql', 'file']],
    '/repo/app/data': [
      ['orders.csv', 'file'],
      ['customers.json', 'file'],
      ['backup.sql', 'file'],
      ['value.txt', 'file'],
    ],
    '/repo/app/证书': [['ca.pem', 'file'], ['client.crt', 'file'], ['client.key', 'file']],
    '/repo/app/空目录': [],
    '/home': [['demo', 'dir']],
    '/home/demo': [],
  };
  const label = (zh, en) => az(zh, en);
  const esc = value => h(value);
  const parent = path => path === '/' ? '/' : path.slice(0, path.lastIndexOf('/')) || '/';
  const join = (dir, name) => (dir === '/' ? '' : dir) + '/' + name;
  let picker = null;
  function buttons(input) {
    if (input.disabled || input.dataset.pathPickerReady) return;
    let modes = [];
    if (input.id === 'query-file-path') {
      modes = [['file', '浏览打开', 'Browse to open'], ['save', '浏览保存', 'Browse to save']];
    } else if (input.id === 'transfer-path') {
      modes = [['file', '浏览导入', 'Browse to import'], ['save', '浏览导出', 'Browse to export']];
    } else if (
      input.matches(
        '[data-connection-field="ca"], [data-connection-field="cert"], [data-connection-field="key"], [data-server-field="ca"]',
      )
    ) modes = [['file', '浏览…', 'Browse…']];
    else if (input.matches('[data-preview-field="path"]') && input.closest('.advanced-preview')) {
      modes = [['file', '浏览导入', 'Browse to import'], ['save', '浏览导出', 'Browse to export']];
    } else if (input.matches('[data-mcp-field="rootPath"]')) modes = [['dir', '浏览目录…', 'Browse directory…']];
    else if (input.matches('[data-xp-field="projectPath"]')) {
      modes = [
        ['dir', '浏览已有目录…', 'Browse existing directory…'],
        ['save', '选择新项目父目录…', 'Select parent directory for new project…'],
      ];
    } else if (input.matches('[data-xp-field="archiveImportPath"]')) {
      modes = [['file', '浏览导入归档…', 'Browse archive to import…']];
    } else if (input.matches('[data-xp-field="archiveExportPath"]')) {
      modes = [['save', '浏览导出位置…', 'Browse archive destination…']];
    } else if (input.matches('[data-xp-collection^="files:"]')) {
      modes = [['file', '浏览…', 'Browse…'], ['multi', '批量选择…', 'Select multiple…']];
    } else if (input.matches('[data-xp-field="target"]') && ['export', 'sql_dump'].includes(xp.mode)) {
      modes = [['save', '浏览…', 'Browse…']];
    }
    if (!modes.length) return;
    if (!input.hasAttribute('aria-label')) {
      const name = input.closest('label')?.textContent.trim();
      if (name) input.setAttribute('aria-label', name);
    }
    input.dataset.pathPickerReady = 'true';
    const row = document.createElement('span');
    row.className = 'path-picker-field';
    input.before(row);
    row.append(input);
    const group = document.createElement('span');
    group.className = 'path-picker-triggers';
    modes.forEach(([mode, zh, en]) => {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'button path-picker-trigger';
      button.textContent = modes.length === 1
        ? label('浏览…', 'Browse…')
        : mode === 'file'
        ? label('打开…', 'Open…')
        : mode === 'multi'
        ? label('批量…', 'Multiple…')
        : mode === 'dir'
        ? label('已有…', 'Existing…')
        : label('保存…', 'Save…');
      button.title = label(zh, en);
      button.setAttribute('aria-label', label(zh, en));
      button.addEventListener('click', event => {
        event.preventDefault();
        event.stopPropagation();
        open(input, button, mode);
      });
      group.append(button);
    });
    row.append(group);
    if (input.id === 'query-file-path') {
      const note = document.createElement('p');
      note.className = 'muted small';
      note.textContent = label(
        '相对路径基准目录（示例）：/repo/app',
        'Relative path base directory (example): /repo/app',
      );
      row.after(note);
    }
  }
  function enhance() {
    document.querySelectorAll(
      '#query-file-path, #transfer-path, [data-connection-field], [data-server-field="ca"], [data-preview-field="path"], [data-mcp-field="rootPath"], [data-xp-field="projectPath"], [data-xp-field="archiveImportPath"], [data-xp-field="archiveExportPath"], [data-xp-field="target"], [data-xp-collection^="files:"]',
    ).forEach(buttons);
  }
  function open(input, trigger, mode) {
    if (picker) return;
    const value = input.value.trim();
    const absolute = value.startsWith('/')
      ? value
      : input.id === 'query-file-path' && value
      ? join('/repo/app', value)
      : '';
    const start = absolute && tree[absolute]
      ? absolute
      : absolute && tree[parent(absolute)]
      ? parent(absolute)
      : '/repo/app';
    let extensions = [];
    if (input.id === 'query-file-path') extensions = ['.sql'];
    else if (input.matches('[data-connection-field], [data-server-field="ca"]')) extensions = ['.pem', '.crt', '.key'];
    else if (input.matches('[data-xp-collection^="files:"]')) {
      extensions = xp.mode === 'sql_restore' ? ['.sql'] : ['.csv', '.json'];
    } else if (input.matches('[data-xp-field="archiveImportPath"]')) extensions = ['.zip'];
    const overlay = document.createElement('dialog');
    overlay.className = 'path-picker-overlay';
    const app = document.getElementById('app');
    const parentDialog = input.closest('dialog[open]');
    if (parentDialog) parentDialog.close();
    app.inert = true;
    document.body.append(overlay);
    overlay.showModal();
    picker = {
      input,
      trigger,
      mode,
      dir: start,
      query: '',
      selected: [],
      current: '',
      extensions,
      overlay,
      app,
      parentDialog,
      error: '',
    };
    render();
    overlay.querySelector('.path-picker-search').focus();
  }
  function entries() {
    const p = picker;
    return (tree[p.dir] || []).filter(([name, kind]) =>
      name.toLowerCase().includes(p.query.toLowerCase())
      && (kind !== 'file' || !p.extensions.length || p.extensions.some(ext => name.toLowerCase().endsWith(ext)))
    );
  }
  function render() {
    const p = picker;
    const rows = entries();
    const title = p.mode === 'multi'
      ? label('选择多个文件', 'Select files')
      : p.mode === 'dir'
      ? label('选择目录', 'Select directory')
      : p.mode === 'save'
      ? label('选择保存位置', 'Select save location')
      : label('选择文件', 'Select file');
    const list = p.error
      ? `<p class="path-picker-status" role="alert">${esc(p.error)}</p>`
      : rows.length
      ? rows.map(([name, kind]) => {
        const path = join(p.dir, name);
        const selected = p.selected.includes(path);
        return `<button type="button" class="path-picker-row" role="option" data-path="${
          esc(path)
        }" data-kind="${kind}" aria-selected="${selected}" tabindex="${
          p.current === path ? '0' : '-1'
        }"><span class="path-picker-icon" aria-hidden="true">${
          icon(kind === 'file' ? 'copy' : 'folder')
        }</span><span class="path-picker-name">${esc(name)}</span><span class="path-picker-kind">${
          esc(kind === 'file' ? label('文件', 'File') : label('目录', 'Directory'))
        }</span>${
          selected ? `<span class="path-picker-check" aria-label="${esc(label('已选择', 'Selected'))}">✓</span>` : ''
        }</button>`;
      }).join('')
      : `<p class="path-picker-status">${
        esc(
          p.query
            ? label('没有匹配的条目。', 'No matching entries.')
            : label('此目录为空。', 'This directory is empty.'),
        )
      }</p>`;
    p.overlay.setAttribute('aria-label', title);
    p.overlay.innerHTML =
      `<section class="path-picker"><header><span class="path-picker-head-icon" aria-hidden="true">${
        icon('folder')
      }</span><h2>${esc(title)}</h2><input class="path-picker-search" aria-label="${
        esc(label('按名称筛选', 'Filter by name'))
      }" placeholder="${esc(label('按名称筛选…', 'Filter by name…'))}" value="${
        esc(p.query)
      }"></header><div class="path-picker-location"><button type="button" class="path-picker-up" data-picker="up" ${
        p.dir === '/' ? 'disabled' : ''
      } title="${esc(label('上一级', 'Parent directory'))}" aria-label="${esc(label('上一级', 'Parent directory'))}">${
        icon('arrow-left')
      }</button><span>${esc(label('当前路径', 'Current path'))}</span><code title="${esc(p.dir)}">${
        esc(p.dir)
      }</code></div><div class="path-picker-list" role="listbox" aria-label="${
        esc(label('目录内容', 'Directory contents'))
      }" ${
        p.mode === 'multi' ? 'aria-multiselectable="true"' : ''
      }>${list}</div><footer><span class="path-picker-note">${
        esc(label('示例数据 · 不访问本机文件', 'Sample data · no local file access'))
      }${
        p.extensions.length ? ` · ${esc(p.extensions.join(' / '))}` : ''
      }</span><button type="button" class="path-picker-cancel" data-picker="cancel">${
        esc(label('取消', 'Cancel'))
      }</button><button type="button" class="primary" data-picker="confirm" ${
        (p.mode === 'file' || p.mode === 'multi') && !p.selected.length ? 'disabled' : ''
      }>${esc(label('确认', 'Confirm'))}</button></footer></section>`;
  }
  function close() {
    const p = picker;
    picker = null;
    p.overlay.remove();
    p.app.inert = false;
    if (p.parentDialog?.isConnected && !p.parentDialog.open) p.parentDialog.showModal();
    (p.trigger.isConnected ? p.trigger : p.input.isConnected ? p.input : document.querySelector('#app button'))
      ?.focus();
  }
  function navigate(path, kind) {
    const p = picker;
    if (kind === 'error') {
      p.error = label('无法读取此目录：权限不足（示例）。', 'Cannot read this directory: permission denied (example).');
      render();
      return;
    }
    if (kind === 'dir') {
      p.dir = path;
      p.query = '';
      p.current = '';
      p.error = '';
      render();
      p.overlay.querySelector('.path-picker-search').focus();
      return;
    }
    p.current = path;
    if (p.mode === 'multi') {
      p.selected = p.selected.includes(path) ? p.selected.filter(item => item !== path) : [...p.selected, path];
    } else p.selected = [path];
    render();
    p.overlay.querySelector(`[data-path="${CSS.escape(path)}"]`)?.focus();
  }
  function confirm() {
    const p = picker;
    if ((p.mode === 'file' || p.mode === 'multi') && !p.selected.length) return;
    if (p.mode === 'multi') {
      xp.files.push(...p.selected);
      xp.prepared = false;
      xp.confirmed = false;
      close();
      renderExchangeProject();
      document.querySelector('[data-xp-collection^="files:"]')?.focus();
      return;
    }
    let value = p.mode === 'file' ? p.selected[0] : p.dir;
    if (p.mode === 'save') {
      const name = p.input.value.trim().split(/[\\/]/).pop();
      value = name ? join(p.dir, name) : p.dir + (p.dir === '/' ? '' : '/');
    }
    p.input.value = value;
    p.input.dispatchEvent(new Event('input', { bubbles: true }));
    close();
  }
  document.addEventListener('click', event => {
    if (!picker || !picker.overlay.contains(event.target)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    const row = event.target.closest('[data-path]');
    const action = event.target.closest('[data-picker]')?.dataset.picker;
    if (row) navigate(row.dataset.path, row.dataset.kind);
    else if (action === 'up') navigate(parent(picker.dir), 'dir');
    else if (action === 'cancel') close();
    else if (action === 'confirm') confirm();
  }, true);
  document.addEventListener('input', event => {
    if (!picker || !event.target.matches('.path-picker-search')) return;
    const position = event.target.selectionStart;
    picker.query = event.target.value;
    picker.error = '';
    render();
    const search = picker.overlay.querySelector('.path-picker-search');
    search.focus();
    search.setSelectionRange(position, position);
  }, true);
  document.addEventListener('keydown', event => {
    if (!picker) return;
    event.stopImmediatePropagation();
    if (event.key === 'Escape') {
      event.preventDefault();
      close();
      return;
    }
    if (event.key === 'Tab') {
      const controls = [...picker.overlay.querySelectorAll('button:not(:disabled), input:not(:disabled)')].filter(
        node => node.tabIndex >= 0,
      );
      const index = controls.indexOf(document.activeElement);
      if (event.shiftKey && index <= 0) {
        event.preventDefault();
        controls.at(-1)?.focus();
      } else if (!event.shiftKey && index === controls.length - 1) {
        event.preventDefault();
        controls[0]?.focus();
      }
      return;
    }
    if (
      ['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)
      && (event.target.matches('.path-picker-search') || event.target.matches('.path-picker-row'))
    ) {
      const rows = [...picker.overlay.querySelectorAll('.path-picker-row')];
      if (!rows.length) return;
      event.preventDefault();
      const index = rows.indexOf(document.activeElement);
      const next = event.key === 'Home'
        ? 0
        : event.key === 'End'
        ? rows.length - 1
        : event.key === 'ArrowDown'
        ? Math.min(index + 1, rows.length - 1)
        : Math.max(index - 1, 0);
      rows.forEach(row => row.tabIndex = -1);
      rows[next].tabIndex = 0;
      rows[next].focus();
    }
  }, true);
  new MutationObserver(enhance).observe(document.getElementById('app'), { childList: true, subtree: true });
  enhance();
})();

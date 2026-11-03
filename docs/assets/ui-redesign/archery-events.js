'use strict';
document.addEventListener('input', event => {
  const field = event.target.dataset.serverField;
  if (serverForm && field) {
    if (field === 'password') {
      serverForm.passwordChanged = !!event.target.value;
    } else if (field === 'totpInput') {
      serverForm.totpChanged = !!event.target.value;
      try {
        const uri = new URL(event.target.value);
        serverForm.totpValid = uri.protocol === 'otpauth:'
          && uri.hostname === 'totp' && !!uri.searchParams.get('secret');
      } catch {
        serverForm.totpValid = false;
      }
      if (serverForm.totpChanged) {
        serverForm.clearTotp = false;
        const clear = document.querySelector('[data-server-clear]');
        if (clear) clear.checked = false;
      }
    } else serverForm[field] = event.target.value;
    if (field === 'url') {
      const ca = document.querySelector('[data-server-field="ca"]'),
        https = event.target.value.toLowerCase().startsWith('https://');
      if (ca) {
        ca.disabled = !https;
        const summary = ca.closest('details').querySelector('summary');
        summary.setAttribute('aria-disabled', String(!https));
        summary.textContent = az('CA 证书', 'CA certificate') + ' · '
          + (https
            ? (serverForm.ca
              ? serverForm.ca.split('/').pop()
              : az('默认信任链', 'Default trust chain'))
            : az('仅 HTTPS 使用', 'HTTPS only'));
      }
    }
  }
  if (event.target.dataset.connectionName) {
    connectionNames[event.target.dataset.connectionName] = event.target.value;
  }
}, true);
document.addEventListener('change', event => {
  if (event.target.id === 'redis-ttl-policy') {
    const required = event.target.value === 'set',
      input = document.getElementById('redis-ttl-value');
    input.required = required;
    input.setAttribute('aria-required', String(required));
    document.getElementById('ttl-required').hidden = !required;
  }
  if (event.target.hasAttribute('data-server-clear') && serverForm) {
    const password = document.querySelector('[data-server-field="password"]')?.value
        || '',
      uri = document.querySelector('[data-server-field="totpInput"]')?.value
        || '';
    serverForm.clearTotp = event.target.checked;
    if (serverForm.clearTotp) {
      serverForm.totpChanged = false;
      serverForm.totpValid = false;
    }
    render();
    document.querySelector('[data-server-field="password"]').value = password;
    if (!serverForm.clearTotp) {
      document.querySelector('[data-server-field="totpInput"]').value = uri;
    }
    document.querySelector('[data-server-clear]').closest('details').open = true;
  }
  if (event.target.hasAttribute('data-save-password') && serverForm) {
    serverForm.savePassword = event.target.checked;
  }
  if (event.target.hasAttribute('data-totp-mode') && serverForm) {
    const password = document.querySelector('[data-server-field="password"]')?.value
        || '',
      uri = document.querySelector('[data-server-field="totpInput"]')?.value
        || '';
    serverForm.totpMode = event.target.value;
    render();
    document.querySelector('[data-totp-mode]').closest('details').open = true;
    document.querySelector('[data-server-field="password"]').value = password;
    document.querySelector('[data-server-field="totpInput"]').value = uri;
  }
  if (event.target.hasAttribute('data-server-select')) {
    beginManagement(event.target.value);
    render();
  }
  if (event.target.id === 'local-history-size') {
    state.historyPageSize = Number(event.target.value);
    state.historyPage = 0;
    render();
  }
}, true);
document.addEventListener('input', event => {
  if (
    !event.target.dataset.af || (previewSource !== 'archery' && !ap.modal)
  ) return;
  const key = event.target.dataset.af;
  ap[key] = event.target.value;
  // 认证绑定改变后清空旧平台候选，避免把旧实例与 DB 带入新服务。
  const bindingChanged = ['url', 'username', 'ca'].includes(key)
    && ap.step !== 'login';
  if (bindingChanged) {
    ap.step = 'login';
    ap.challenge = false;
    ap.password = '';
    ap.otp = '';
    ap.selectedInstances.clear();
    Object.keys(ap.selected).forEach(id => {
      ap.selected[id] = [];
    });
    ap.defaults = {};
  }
  if (
    bindingChanged
    || ['search', 'instanceSearch', 'objectFilter'].includes(key)
  ) {
    if (key === 'filter') {
      ap.page = 0;
      ap.selectedHistory = 0;
    }
    const pos = event.target.selectionStart;
    render();
    const field = document.querySelector(`[data-af="${key}"]`);
    field?.focus();
    if (typeof pos === 'number') field?.setSelectionRange(pos, pos);
  }
}, true);
document.addEventListener('select', event => {
  if (
    previewSource !== 'archery' || !event.target.matches('.archery-editor')
  ) return;
  const button = document.querySelector('[data-archery="selection"]');
  if (button) {
    button.disabled = ap.task === 'running'
      || event.target.selectionStart === event.target.selectionEnd;
  }
}, true);
document.addEventListener('change', event => {
  if (previewSource !== 'archery' && !ap.modal) return;
  if (event.target.id === 'preview-provider') {
    switchArcheryInstance(
      activeServerId,
      event.target.value === 'redis' ? 'redis-a' : 'mysql-a',
    );
    render();
    event.stopImmediatePropagation();
    return;
  }
  if (event.target.hasAttribute('data-review-preview')) {
    ap.check = event.target.value;
    render();
    return;
  }
  if (event.target.hasAttribute('data-cli-page-size')) {
    const entry = ap.cliEntries[ap.cliSelected ?? ap.cliEntries.length - 1];
    if (entry) {
      entry.pageSize = Number(event.target.value);
      entry.page = 0;
    }
    render();
    return;
  }
  if (event.target.hasAttribute('data-request-limit')) {
    // 仅影响下次显式执行，保留现有缓存、执行时上限和页码。
    ap.requestLimit = Number(event.target.value);
    render();
    return;
  }
  if (event.target.hasAttribute('data-result-page-size')) {
    ap.resultPageSize = Number(event.target.value);
    ap.resultPage = 0;
    render();
    return;
  }
  if (event.target.dataset.ac) {
    ap[event.target.dataset.ac] = event.target.checked;
  }
  if (event.target.hasAttribute('data-page-size')) {
    ap.pageSize = Number(event.target.value);
    ap.page = 0;
    ap.selectedHistory = 0;
    render();
  }
  if (event.target.hasAttribute('data-at')) {
    ap.db = event.target.value;
    const tab = ap.tabs.find(item => item.id === ap.active);
    if (tab) tab.database = ap.db;
    ap.result = false;
    render();
  }
  if (event.target.dataset.ai) {
    const id = event.target.dataset.ai;
    managementFocus = id;
    if (event.target.checked) {
      const original = managementBaseline.find(c => c.instance === id);
      if (original) {
        connectionNames[id] = original.name;
        ap.selected[id] = [...original.dbs];
        ap.defaults[id] = original.defaultDb;
      }
      ap.selectedInstances.add(id);
      if (!ap.selected[id].length) {
        ap.selected[id] = [ai.find(item => item.id === id).dbs[0]];
        ap.defaults[id] = ap.selected[id][0];
      }
    } else {
      ap.selectedInstances.delete(id);
    }
    render();
  }
  if (event.target.dataset.ad) {
    const [id, db] = event.target.dataset.ad.split(':');
    managementFocus = id;
    ap.selected[id] = event.target.checked
      ? [...ap.selected[id], db]
      : ap.selected[id].filter(v => v !== db);
    if (!ap.selected[id].includes(ap.defaults[id])) {
      ap.defaults[id] = ap.selected[id][0] || '';
    }
    render();
  }
}, true);
function positionResultPopup() {
  const popup = document.getElementById('archery-result-details');
  const button = document.querySelector('[data-result-info]');
  if (!popup?.matches(':popover-open') || !button) return;
  const anchor = button.getBoundingClientRect();
  const margin = 12, gap = 6;
  const below = innerHeight - anchor.bottom - gap - margin;
  const above = anchor.top - gap - margin;
  const placeBelow = below >= Math.min(popup.scrollHeight, 360)
    || below >= above;
  popup.style.maxHeight = `${Math.max(0, Math.min(360, placeBelow ? below : above))}px`;
  const bounds = popup.getBoundingClientRect();
  popup.style.left = `${
    Math.max(
      margin,
      Math.min(
        anchor.right - bounds.width,
        innerWidth - bounds.width - margin,
      ),
    )
  }px`;
  popup.style.top = `${placeBelow ? anchor.bottom + gap : anchor.top - gap - bounds.height}px`;
}
function closeResultPopup() {
  document.getElementById('archery-result-details')?.hidePopover();
  const button = document.querySelector('[data-result-info]');
  button?.setAttribute('aria-expanded', 'false');
  button?.focus();
}
document.addEventListener('toggle', event => {
  if (event.target.id !== 'archery-result-details') return;
  document.querySelector('[data-result-info]')?.setAttribute(
    'aria-expanded',
    String(event.newState === 'open'),
  );
  if (event.newState === 'open') positionResultPopup();
}, true);
document.addEventListener('keydown', event => {
  if (
    event.key === 'Escape'
    && document.getElementById('archery-result-details')?.matches(
      ':popover-open',
    )
  ) {
    event.preventDefault();
    event.stopImmediatePropagation();
    closeResultPopup();
  }
}, true);
window.addEventListener('resize', positionResultPopup);
document.addEventListener('keydown', event => {
  if (
    (event.key === 'Enter' || event.key === ' ')
    && event.target.matches(
      '[data-archery^="cli-locate:"], [data-cli-locate]',
    )
  ) {
    event.preventDefault();
    event.target.click();
  }
}, true);
document.addEventListener('click', event => {
  const entry = event.target.closest('[data-cli-locate]');
  if (!entry) return;
  state.cliSelected = Number(entry.dataset.cliLocate);
  render();
}, true);
document.addEventListener('click', event => {
  if (!event.target.closest('[data-action="close-modal"]')) return;
  const overlays = document.querySelectorAll('#app .overlay');
  if (event.target.closest('.overlay') !== overlays[overlays.length - 1]) {
    return;
  }
  if (
    serverForm || serverDelete || connectionDelete || authStage || ap.modal
  ) {
    const child = serverForm || serverDelete || connectionDelete
      || authStage;
    event.stopImmediatePropagation();
    serverForm = null;
    serverDelete = null;
    connectionDelete = null;
    authStage = null;
    if (!child && ap.modal) ap.modal = false;
    render();
  } else if (
    previewSource === 'archery' && ap.view === 'history' && !state.overlay
  ) {
    event.stopImmediatePropagation();
    ap.view = ap.historyReturn || 'query';
    render();
  }
}, true);
document.addEventListener('click', event => {
  const target = event.target.closest('[data-archery]');
  if (!target) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  const [action, id, db] = target.dataset.archery.split(':');
  const targetServer = target.dataset.connectionServer || activeServerId;
  if (action === 'ca-clear' && serverForm) {
    const password = document.querySelector('[data-server-field="password"]')?.value
        || '',
      uri = document.querySelector('[data-server-field="totpInput"]')?.value
        || '';
    serverForm.ca = '';
    render();
    document.querySelector('[data-server-field="password"]').value = password;
    document.querySelector('[data-server-field="totpInput"]').value = uri;
    document.querySelector('[data-server-field="ca"]').closest('details')
      .open = true;
    return;
  }
  if (action === 'server-add' || action === 'server-add-settings') {
    openServerForm(null, action === 'server-add' ? 'wizard' : 'settings');
    render();
    return;
  }
  if (action === 'server-edit') {
    openServerForm(id, ap.modal ? 'wizard' : 'settings');
    render();
    return;
  }
  if (action === 'server-cancel') {
    serverForm = null;
    render();
    return;
  }
  if (action === 'server-save') {
    const f = serverForm, old = servers.find(s => s.id === f.id);
    let normalized;
    clearFieldErrors();
    for (const key of ['name', 'url', 'username']) {
      if (!f[key].trim()) {
        fieldError(
          document.querySelector(`[data-server-field="${key}"]`),
          az('此项不能为空。', 'This field is required.'),
        );
        return;
      }
    }
    if (!f.editing && !f.passwordSaved && !f.passwordChanged) {
      fieldError(
        document.querySelector('[data-server-field="password"]'),
        az('请输入密码。', 'Enter a password.'),
      );
      return;
    }
    try {
      const url = new URL(f.url);
      if (
        !['http:', 'https:'].includes(url.protocol) || url.username
        || url.password || url.search || url.hash
      ) throw Error();
      normalized = url.origin + url.pathname.replace(/\/+$/, '');
    } catch {
      fieldError(
        document.querySelector('[data-server-field="url"]'),
        az('请输入有效的服务器地址。', 'Enter a valid server URL.'),
      );
      return;
    }
    if (
      (f.totpChanged && !f.totpValid)
      || (f.totpMode === 'automatic' && !f.totpChanged
        && (!f.totp || f.clearTotp))
    ) {
      fieldError(
        document.querySelector('[data-server-field="totpInput"]'),
        az('请输入有效的 TOTP 链接。', 'Enter a valid TOTP URI.'),
      );
      return;
    }
    if (
      servers.some(s =>
        s.id !== f.id && s.url.replace(/\/+$/, '') === normalized
        && s.username === f.username.trim()
      )
    ) {
      fieldError(
        document.querySelector('[data-server-field="url"]'),
        az(
          '相同地址和账号已存在。',
          'This address and account already exist.',
        ),
      );
      return;
    }
    if (
      ap.task === 'running'
      && (f.passwordChanged || f.totpChanged || f.clearTotp
        || (old
          && (old.ca !== f.ca || old.totpMode !== f.totpMode
            || old.passwordSaved !== f.savePassword)))
    ) {
      toast(
        az(
          '任务运行中，无法保存认证修改。',
          'Authentication changes cannot be saved while a task is running.',
        ),
      );
      return;
    }
    const data = {
      id: f.id || 's' + Date.now(),
      name: f.name.trim(),
      url: normalized,
      username: f.username.trim(),
      ca: normalized.startsWith('https:') ? f.ca : '',
      totp: f.clearTotp ? false : (f.totpChanged || f.totp),
      totpMode: f.totpMode,
      passwordSaved: f.savePassword
        && (f.passwordChanged || f.passwordSaved || false),
      requiresOtp: old?.requiresOtp ?? true,
      loggedIn: old?.loggedIn || false,
    };
    if (old) Object.assign(old, data);
    else servers.push(data);
    serverId = data.id;
    serverForm = null;
    if (serverReturn === 'wizard' && !old) beginManagement(serverId);
    render();
    return;
  }
  if (action === 'server-delete') {
    serverDelete = id;
    render();
    return;
  }
  if (action === 'server-delete-cancel') {
    serverDelete = null;
    render();
    return;
  }
  if (action === 'server-delete-confirm') {
    if (
      ap.task === 'running' || ap.draft || state.tabs.some(tab => tab.running) || state.drafts.length
      || currentDrafts().length
    ) {
      toast(
        az(
          '存在任务或草稿，无法删除。',
          'A running task or draft blocks deletion.',
        ),
      );
      return;
    }
    for (let n = archeryConnections.length - 1; n >= 0; n--) {
      if (archeryConnections[n].server === serverDelete) {
        archeryConnections.splice(n, 1);
      }
    }
    servers.splice(servers.findIndex(s => s.id === serverDelete), 1);
    serverId = servers[0]?.id || '';
    if (activeServerId === serverDelete) activeServerId = serverId;
    serverDelete = null;
    render();
    return;
  }
  if (action === 'refresh-instances') {
    const s = servers.find(item => item.id === serverId);
    if (!s) return;
    if (
      s.loggedIn
      || (s.passwordSaved
        && (!s.requiresOtp || (s.totp && s.totpMode === 'automatic')))
    ) {
      s.loggedIn = true;
      refreshedServers.add(serverId);
      toast(az('实例已刷新（示例）。', 'Instances refreshed (example).'));
    } else {
      authStage = s.passwordSaved ? 'otp' : 'password';
      authReturn = 'refresh';
    }
    render();
    return;
  }
  if (action === 'auth-cancel') {
    authStage = null;
    authReturn = null;
    toast(
      az(
        '认证已取消，未刷新实例。',
        'Authentication cancelled; instances were not refreshed.',
      ),
    );
    render();
    return;
  }
  if (action === 'auth-next') {
    const value = document.querySelector('[data-auth-input]')?.value || '';
    if (
      !value.trim()
      || (authStage === 'otp' && !/^\d{6}$/.test(value.trim()))
    ) {
      fieldError(
        document.querySelector('[data-auth-input]'),
        az(
          '请填写密码或六位验证码。',
          'Enter the password or six-digit verification code.',
        ),
      );
      return;
    }
    const s = servers.find(item => item.id === serverId);
    if (
      authStage === 'password' && s?.requiresOtp
      && !(s.totp && s.totpMode === 'automatic')
    ) authStage = 'otp';
    else {
      if (s) s.loggedIn = true;
      authStage = null;
      if (authReturn === 'refresh') {
        refreshedServers.add(serverId);
        authReturn = null;
        toast(az('实例已刷新（示例）。', 'Instances refreshed (example).'));
      }
      if (ap.reauth) {
        ap.modal = false;
        ap.reauth = false;
      }
    }
    render();
    return;
  }
  if (action === 'connection-edit') {
    openArcheryEdit(id, targetServer);
    render();
    return;
  }
  if (action === 'connection-copy') {
    const c = archeryConnections.find(item => item.server === targetServer && item.instance === id);
    if (c) copy(c.name);
    state.menu = null;
    render();
    return;
  }
  if (action === 'connection-delete') {
    state.menu = null;
    connectionDelete = { server: targetServer, instance: id };
    render();
    return;
  }
  if (action === 'connection-delete-cancel') {
    connectionDelete = null;
    render();
    return;
  }
  if (action === 'connection-delete-confirm') {
    if (ap.task === 'running' || ap.draft) {
      toast(
        az(
          '存在任务或草稿，无法删除。',
          'A running task or draft blocks deletion.',
        ),
      );
      return;
    }
    const index = archeryConnections.findIndex(item =>
      item.server === connectionDelete.server
      && item.instance === connectionDelete.instance
    );
    if (index >= 0) archeryConnections.splice(index, 1);
    connectionDelete = null;
    render();
    return;
  }
  if (action === 'direct') {
    previewSource = 'direct';
    $('#preview-source').value = 'direct';
    $('#preview-state').value = 'default';
    translatePreview();
  } else if (action === 'otp-visible') ap.otpVisible = !ap.otpVisible;
  else if (action === 'type-archery' || action === 'wizard') {
    state.overlay = null;
    ap.reauth = false;
    beginManagement(serverId);
  } else if (action === 'type-mysql' || action === 'type-redis') {
    ap.modal = false;
    openConnectionForm('new-connection', action.slice(5));
    return;
  } else if (action === 'close') ap.modal = false;
  else if (action === 'new' || action === 'saved') {
    ap.mode = action;
    ap.step = 'login';
    ap.challenge = false;
    ap.password = '';
    ap.otp = '';
    if (action === 'new') {
      ap.reauth = false;
      ap.selectedInstances.clear();
      Object.keys(ap.selected).forEach(id => {
        ap.selected[id] = [];
      });
      ap.defaults = {};
    }
  } else if (action === 'login' || action === 'verify') {
    ap.password = '';
    ap.otp = '';
    ap.challenge = false;
    if (ap.reauth) {
      ap.modal = false;
      ap.reauth = false;
    } else ap.step = 'instances';
  } else if (action === 'load-instances') {
    toast(
      az(
        '示例实例已展示；未发送请求。',
        'Example instances shown; no request sent.',
      ),
    );
  } else if (action === 'select-instances') {
    ai.filter(item =>
      item.name.toLowerCase().includes(ap.instanceSearch.toLowerCase())
      && !archeryConnections.some(c => c.server === serverId && c.instance === item.id)
    ).forEach(item => {
      ap.selectedInstances.add(item.id);
      if (!ap.selected[item.id].length) {
        ap.selected[item.id] = [item.dbs[0]];
        ap.defaults[item.id] = item.dbs[0];
      }
    });
  } else if (action === 'load') {
    toast(
      az(
        '示例数据库已展示；未发送请求。',
        'Example databases shown; no request sent.',
      ),
    );
  } else if (
    action === 'all' || action === 'clear' || action === 'remove'
  ) {
    const matches = ai.find(i => i.id === id).dbs.filter(v => v.toLowerCase().includes(ap.search.toLowerCase()));
    ap.selected[id] = action === 'remove'
      ? []
      : action === 'all'
      ? [...new Set([...ap.selected[id], ...matches])]
      : ap.selected[id].filter(v => !matches.includes(v));
    if (!ap.selected[id].includes(ap.defaults[id])) {
      ap.defaults[id] = ap.selected[id][0] || '';
    }
    if (action === 'remove') ap.selectedInstances.delete(id);
  } else if (action === 'default') {
    ap.defaults[id] = db;
    managementFocus = id;
  } else if (action === 'preview') {
    clearFieldErrors();
    for (
      const item of managementInstances().filter(i => ap.selectedInstances.has(i.id))
    ) {
      if (!connectionNames[item.id].trim()) {
        fieldError(
          document.querySelector(`[data-connection-name="${item.id}"]`),
          az('请输入连接名称。', 'Enter a connection name.'),
        );
        return;
      }
      if (
        !ap.selected[item.id].length
        || !ap.selected[item.id].includes(ap.defaults[item.id])
      ) {
        fieldError(
          document.querySelector(`[data-ad^="${item.id}:"]`),
          az(
            '请选择数据库和范围内默认数据库。',
            'Select a database and a default within the scope.',
          ),
        );
        return;
      }
    }
    const c = managementChanges();
    if (!c.added.length && !c.modified.length && !c.removed.length) {
      toast(az('没有需要保存的变更。', 'There are no changes to save.'));
      return;
    }
    ap.step = 'preview';
  } else if (action === 'back') ap.step = 'instances';
  else if (action === 'save') {
    const c = managementChanges();
    if (
      serverId === activeServerId && [...c.modified, ...c.removed].some(v => v.instance === ap.instance)
      && (ap.task === 'running' || ap.draft)
    ) {
      toast(
        az(
          '受影响连接存在任务或草稿，整批未保存。',
          'A task or draft on an affected connection blocks the entire batch.',
        ),
      );
      return;
    }
    if (
      [...c.modified, ...c.removed].some(v => occupiedConnections.has(`${serverId}:${v.instance}`))
    ) {
      toast(
        az(
          '其他窗口正在使用受影响连接，整批未保存。',
          'Another window is using an affected connection; the entire batch was not saved.',
        ),
      );
      return;
    }
    if (
      JSON.stringify(archeryConnections.filter(v => v.server === serverId))
        !== JSON.stringify(managementBaseline)
    ) {
      toast(
        az(
          '配置已变化，整批未保存。',
          'Configuration changed; the batch was not saved.',
        ),
      );
      return;
    }
    archeryConnections.splice(
      0,
      archeryConnections.length,
      ...archeryConnections.filter(v => v.server !== serverId),
      ...c.desired,
    );
    ap.modal = false;
    if (c.desired.length) {
      activeServerId = serverId;
      previewSource = 'archery';
      $('#preview-source').value = 'archery';
      ap.instance = c.desired[0].instance;
      ap.db = c.desired[0].defaultDb;
      translatePreview();
    } else if (activeServerId === serverId) {
      ap.tabs = [];
      ap.active = null;
      ap.result = false;
      ap.view = 'query';
    }
  } else if (action === 'tree') {
    ap.db = id;
    const activeTab = ap.tabs.find(tab => tab.id === ap.active && tab.query);
    if (activeTab) activeTab.database = id;
    ap.result = false;
    ap.expandedDatabases.has(id)
      ? ap.expandedDatabases.delete(id)
      : ap.expandedDatabases.add(id);
  } else if (action === 'table') {
    ap.db = id;
    saveArcheryTab();
    let tab = ap.tabs.find(item => item.query === false && item.database === ap.db);
    if (!tab) {
      tab = {
        id: ap.nextTab++,
        title: 'orders',
        query: false,
        database: ap.db,
      };
      ap.tabs.push(tab);
    }
    activateArcheryTab(tab);
  } else if (action === 'new-tab') {
    saveArcheryTab();
    const number = ap.nextTab++;
    const tab = {
      id: number,
      number,
      title: 'Query ' + number,
      query: true,
      database: ap.db,
    };
    ap.tabs.push(tab);
    activateArcheryTab(tab);
  } else if (action === 'tab') {
    saveArcheryTab();
    const tab = ap.tabs.find(tab => tab.id === Number(id));
    activateArcheryTab(tab);
  } else if (action === 'close-tab') {
    const index = ap.tabs.findIndex(tab => tab.id === Number(id));
    if (index >= 0) {
      const active = ap.active === Number(id);
      ap.tabs.splice(index, 1);
      if (active) {
        activateArcheryTab(ap.tabs[Math.min(index, ap.tabs.length - 1)]);
      }
    }
  } else if (action === 'close-result') ap.result = false;
  else if (action === 'copy-result') {
    const text = document.querySelector('.archery-cli')
      ? ap.cliEntries.map(entry =>
        `${entry.instance} · DB ${entry.database}\n› ${entry.command}\n${
          (entry.values ? entry.values.join('\n') : entry.text) || ''
        }`
      ).join('\n\n')
      : (ap.review
        ? [...document.querySelectorAll('.sql-results .table-wrap tr')]
          .map(row => [...row.cells].map(cell => cell.textContent).join('\t')).join('\n')
        : [['id', 'status', 'total'], ...ap.cachedRows].map(row => row.join('\t')).join('\n'));
    if (text) copy(text);
  } else if (action === 'export-result') {
    toast(
      ai.find(item => item.id === ap.instance).provider === 'redis'
        ? az(
          '导出定位的缓存结果：CSV / JSON；设计预览不生成文件。',
          'Export the located cached result as CSV / JSON; this preview creates no files.',
        )
        : az(
          '导出本次缓存结果：CSV / JSON；设计预览不生成文件。',
          'Export the current cached result as CSV / JSON; this preview creates no files.',
        ),
    );
  } else if (action === 'file') {
    toast(
      az(
        '示例预览不读取文件。',
        'The example preview does not read files.',
      ),
    );
  } else if (action === 'explain') {
    toast(
      az(
        '示例预览没有执行计划，未发送请求。',
        'No execution plan in this preview; no request sent.',
      ),
    );
  } else if (action === 'instance') {
    switchArcheryInstance(id, db);
    serverId = id;
    if (ap.historyServer !== id) {
      ap.filter = '';
      ap.historyApplied = '';
      ap.page = 0;
      ap.selectedHistory = 0;
      ap.historyServer = id;
    }
  } else if (action === 'check') {
    ap.view = 'query';
    ap.review = true;
    ap.result = true;
    ap.reviewDetail = false;
  } else if (['query', 'schema', 'ddl', 'history'].includes(action)) {
    if (action === 'history') ap.historyReturn = ap.view;
    ap.view = action === 'query' && ap.view === 'history'
      ? (ap.historyReturn || 'query')
      : action;
    if (action === 'history') {
      ap.page = 0;
      ap.filter = '';
      ap.historyApplied = '';
    }
    ap.selectedHistory = 0;
  } else if (action === 'result' || action === 'selection') {
    ap.result = true;
    ap.review = false;
    ap.task = 'idle';
    ap.cliSelected = ap.cliEntries.length;
    ap.draft = false;
    if (ai.find(item => item.id === ap.instance).provider === 'redis') {
      if (!ap.command.trim()) return;
      appendCliRecord(ap.cliEntries, {
        command: ap.command,
        database: ap.db,
        instance: ai.find(item => item.id === ap.instance).name,
        status: 'success',
        ms: 1,
        limit: ap.requestLimit,
        page: 0,
        pageSize: 20,
        text: az(
          '示例返回；未发送命令。',
          'Example response; no command sent.',
        ),
      });
    } else {
      ap.executedLimit = ap.requestLimit;
      ap.cachedRows = archeryQuerySamples.slice(0, ap.executedLimit);
      ap.resultPage = 0;
    }
  } else if (action === 'cli-locate') {
    ap.cliSelected = Number(id);
  } else if (action === 'cli-prev' || action === 'cli-next') {
    const entry = ap.cliEntries[ap.cliSelected ?? ap.cliEntries.length - 1];
    if (entry) {
      entry.page = (entry.page ?? 0) + (action === 'cli-prev' ? -1 : 1);
    }
  } else if (action === 'result-prev' || action === 'result-next') {
    ap.resultPage += action === 'result-prev' ? -1 : 1;
  } else if (action === 'stop') ap.task = 'unknown';
  else if (action === 'review-detail') ap.reviewDetail = !ap.reviewDetail;
  else if (action.startsWith('check-')) ap.check = action.slice(6);
  else if (action === 'previous' || action === 'next') {
    ap.page += action === 'previous' ? -1 : 1;
    ap.selectedHistory = 0;
  } else if (action === 'history-query') {
    ap.historyApplied = ap.filter;
    ap.page = 0;
    ap.selectedHistory = 0;
  } else if (action === 'history-select') ap.selectedHistory = Number(id);
  else if (action === 'open' && ap.task !== 'running') {
    const row = ahistory()[ap.page * ap.pageSize + ap.selectedHistory];
    if (row) {
      saveArcheryTab();
      const number = ap.nextTab++;
      const tab = {
        id: number,
        number,
        title: 'Query ' + number,
        query: true,
        database: ap.db,
      };
      ap.tabs.push(tab);
      activateArcheryTab(tab);
      if (ap.instance === 'redis-a') ap.command = row.text;
      else ap.sql = row.text;
      ap.result = false;
      ap.draft = true;
    }
  }
  render();
}, true);

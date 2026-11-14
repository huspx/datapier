'use strict';
let authExampleState = '';
let serverDeleteSnapshot = null;
let recoveryReviewed = false;
function selectArcheryPreview(kind) {
  recoveryReviewed = false;
  state.overlay = null;
  state.menu = null;
  ap.view = 'query';
  connectionDelete = null;
  ap.modal = false;
  ap.task = 'idle';
  serverForm = null;
  serverDelete = null;
  authStage = null;
  authExampleState = '';
  if (['archery-delete-busy', 'archery-delete-failed', 'archery-delete-unknown'].includes(kind)) {
    authExampleState = kind;
    serverDelete = serverId;
    serverDeleteSnapshot = {
      server: { ...servers.find(s => s.id === serverId) },
      connections: archeryConnections.filter(c => c.server === serverId).map(c => ({ ...c, dbs: [...c.dbs] })),
    };
    return;
  }
  if (['archery-auth-busy', 'archery-auth-observe', 'archery-auth-failed', 'archery-save-unknown'].includes(kind)) {
    beginManagement(serverId);
    authExampleState = kind;
    if (kind === 'archery-save-unknown') ap.step = 'preview';
    else authStage = 'password';
    return;
  }
  occupiedConnections.clear();
  unavailableInstances.clear();
  if (kind === 'archery-management-busy') {
    beginManagement('s1', 'mysql-a');
    connectionNames['mysql-a'] += '-preview';
    occupiedConnections.add('s1:mysql-a');
    return;
  }
  if (kind === 'archery-management-missing') {
    unavailableInstances.add('s1:redis-a');
    refreshedServers.add('s1');
    beginManagement('s1', 'redis-a');
    return;
  }
  if (kind === 'archery-server') {
    state.overlay = 'app-settings';
    return;
  }
  if (kind === 'archery-server-edit') {
    openServerForm('s1', 'settings');
    return;
  }
  if (kind === 'archery-edit') {
    openArcheryEdit('mysql-a');
    return;
  }
  if (kind === 'archery-preview') {
    beginManagement('s2');
    refreshedServers.add('s2');
    for (const item of ai) {
      ap.selectedInstances.add(item.id);
      ap.selected[item.id] = [...item.dbs];
      ap.defaults[item.id] = item.dbs[0];
    }
    ap.step = 'preview';
    return;
  }
  ap.reauth = false;
  if (
    ['archery-login', 'archery-totp', 'archery-instances', 'archery-auth']
      .includes(kind)
  ) {
    beginManagement(serverId);
    ap.reauth = kind === 'archery-auth';
    if (kind !== 'archery-instances') {
      authStage = kind === 'archery-totp' ? 'otp' : 'password';
      authReturn = 'wizard';
    }
  } else if (kind === 'archery-redis') {
    switchArcheryInstance(activeServerId, 'redis-a');
    ap.db = '0';
    ap.command = 'GET example:key';
    if (!ap.cliEntries.length) {
      ap.cliEntries = archeryCliSamples.map((entry, index) => ({
        ...entry,
        limit: 100,
        page: 0,
        pageSize: 20,
        values: index
          ? [entry.text]
          : Array.from({ length: 45 }, (_, n) => `example:value:${n + 1}`),
      }));
      ap.cliSelected = ap.cliEntries.length - 1;
    }
    const tab = ap.tabs.find(item => item.id === ap.active);
    if (tab) tab.database = ap.db;
    ap.task = 'idle';
    ap.view = 'query';
    ap.result = true;
    ap.review = false;
  } else {
    switchArcheryInstance(activeServerId, 'mysql-a');
    ap.db = 'commerce';
    if (!ap.sql) ap.sql = 'SELECT id, status, total FROM orders;';
    const tab = ap.tabs.find(item => item.id === ap.active);
    if (tab) tab.database = ap.db;
    ap.task = 'idle';
    ap.view = kind === 'archery-history' ? 'history' : 'query';
    if (kind === 'archery-history') {
      ap.filter = '';
      ap.historyApplied = '';
      ap.page = 0;
      ap.selectedHistory = 0;
    }
    ap.result = true;
    ap.review = kind === 'archery-check';
    if (kind === 'archery-denied') ap.task = 'denied';
    if (kind === 'archery-running') ap.task = 'running';
    if (kind === 'archery-unknown') ap.task = 'unknown';
  }
}
function serverList() {
  return `<div class="row"><h3>${az('Archery 服务器', 'Archery servers')}</h3><span class="spacer"></span>${
    ab(
      icon('plus'),
      'server-add-settings',
      `class="icon-button" title="${az('新增服务器', 'Add server')}" aria-label="${az('新增服务器', 'Add server')}"`,
    )
  }</div>${
    servers.map(s =>
      `<div class="archery-group config-server-group"><div class="config-server-info">${
        configRow(az('服务名称', 'Server name'), `<span class="server-list-name">${h(s.name)}</span>`)
      }${configRow(az('服务地址', 'Server address'), `<span>${h(s.url)}</span>`)}${
        configRow(az('用户名', 'Username'), `<span>${h(s.username)}</span>`)
      }${
        configRow(
          az('关联连接', 'Connections'),
          `<span>${archeryConnections.filter(c => c.server === s.id).length}</span>`,
        )
      }</div><div class="config-server-actions"><button class="icon-button" data-archery="server-edit:${s.id}" title="${
        az('编辑', 'Edit')
      }" aria-label="${az('编辑服务器', 'Edit server')}">${
        icon('edit')
      }</button><button class="icon-button danger" data-archery="server-delete:${s.id}" title="${
        az('删除', 'Delete')
      }" aria-label="${az('删除服务器', 'Delete server')}">${icon('trash')}</button></div></div>`
    ).join('')
  }`;
}
function openServerForm(id, origin) {
  const s = servers.find(item => item.id === id);
  serverReturn = origin;
  serverForm = s
    ? {
      ...s,
      editing: true,
      password: '',
      savePassword: s.passwordSaved,
      saveTotp: s.saveTotp ?? !!s.totp,
      totpInput: '',
      clearTotp: false,
    }
    : {
      name: '',
      url: 'https://',
      username: '',
      ca: '',
      totp: false,
      totpMode: 'manual',
      savePassword: true,
      saveTotp: true,
      password: '',
      totpInput: '',
      clearTotp: false,
      editing: false,
    };
}
let managementBaseline = [],
  managementFocus = '',
  managementLocked = false,
  refreshedServers = new Set();
const unavailableInstances = new Set(), occupiedConnections = new Set();
function managementInstances() {
  const discovered = refreshedServers.has(serverId)
    ? ai.filter(item => !unavailableInstances.has(`${serverId}:${item.id}`))
    : [];
  const resources = new Map(managementBaseline.map(c => [c.instance, {
    id: c.instance,
    name: c.instanceName,
    provider: c.provider,
    dbs: [...c.dbs],
    unavailable: refreshedServers.has(serverId)
      && !discovered.some(item => item.id === c.instance),
  }]));
  for (const item of discovered) {
    resources.set(item.id, { ...item, unavailable: false });
  }
  return [...resources.values()];
}
function beginManagement(sid, focus = '') {
  serverId = sid;
  managementFocus = focus;
  managementLocked = !!focus;
  managementBaseline = archeryConnections.filter(c => c.server === sid).map(
    c => ({ ...c, dbs: [...c.dbs] }),
  );
  ap.selectedInstances = new Set(managementBaseline.map(c => c.instance));
  const resources = new Map(
    [...ai, ...managementInstances()].map(item => [item.id, item]),
  );
  resources.forEach(item => {
    const c = managementBaseline.find(v => v.instance === item.id);
    connectionNames[item.id] = c?.name || item.name;
    ap.selected[item.id] = c ? [...c.dbs] : [];
    ap.defaults[item.id] = c?.defaultDb || '';
  });
  ap.modal = true;
  ap.step = 'instances';
}
function managementChanges() {
  const desired = managementInstances().filter(i => ap.selectedInstances.has(i.id)).map(i => ({
    ...(managementBaseline.find(c => c.instance === i.id) || {}),
    server: serverId,
    instance: i.id,
    instanceName: i.name,
    provider: i.provider,
    name: (connectionNames[i.id] || i.name).trim(),
    dbs: [...(ap.selected[i.id] || [])],
    defaultDb: ap.defaults[i.id],
  }));
  const added = desired.filter(c => !managementBaseline.some(b => b.instance === c.instance));
  const removed = managementBaseline.filter(c => !desired.some(d => d.instance === c.instance));
  const modified = desired.filter(c => {
    const b = managementBaseline.find(v => v.instance === c.instance);
    return b
      && (b.name !== c.name || b.defaultDb !== c.defaultDb
        || JSON.stringify(b.dbs) !== JSON.stringify(c.dbs));
  });
  return { desired, added, removed, modified };
}
function openArcheryEdit(id, sid = activeServerId) {
  state.menu = null;
  state.overlay = null;
  beginManagement(sid, id);
  requestAnimationFrame(() =>
    document.querySelector(`[data-ai="${id}"]`)?.scrollIntoView({
      block: 'nearest',
    })
  );
}
function archeryWizard() {
  if (
    !ap.modal || serverForm || serverDelete || connectionDelete || authStage
  ) return '';
  const server = servers.find(s => s.id === serverId),
    changes = managementChanges();
  let body = `${
    managementLocked
      ? ''
      : `<div class="provider-options" role="radiogroup" aria-label="${
        t('databaseType')
      }"><button class="provider-option" role="radio" aria-checked="false" data-archery="type-mysql" ${
        ap.step === 'preview' ? 'disabled' : ''
      }>${
        marker('mysql')
      }<span>MySQL</span></button><button class="provider-option" role="radio" aria-checked="false" data-archery="type-redis" ${
        ap.step === 'preview' ? 'disabled' : ''
      }>${
        marker('redis')
      }<span>Redis</span></button><button class="provider-option" role="radio" aria-checked="true" tabindex="0" data-archery="type-current" ${
        ap.step === 'preview' ? 'disabled' : ''
      }><img class="archery-mark" src="assets/archery.svg" alt=""><span>Archery</span></button></div>`
  }<p class="archery-note">${
    az(
      '交互示例：不发送请求或保存真实凭据。',
      'Interactive example: no requests or real credentials are saved.',
    )
  }</p><nav class="management-steps" aria-label="${
    az('连接配置步骤', 'Connection configuration steps')
  }"><span class="management-step" ${ap.step === 'instances' ? 'aria-current="step"' : ''}>1. ${
    az('配置', 'Configure')
  }</span><span class="management-step" ${ap.step === 'preview' ? 'aria-current="step"' : ''}>2. ${
    az('预览', 'Preview')
  }</span></nav><div class="archery-actions server-selector"><label><span>${
    az('服务器', 'Server')
  } <span class="required-mark" aria-hidden="true">*</span></span><select data-server-select required aria-required="true" ${
    managementLocked || ap.step === 'preview' ? 'disabled' : ''
  }>${
    servers.map(s =>
      `<option value="${h(s.id)}" ${s.id === serverId ? 'selected' : ''}>${h(s.name)} · ${h(s.username)}</option>`
    ).join('')
  }</select></label>${
    managementLocked || ap.step === 'preview'
      ? ''
      : ab(
        icon('plus'),
        'server-add',
        `class="icon-button" title="${az('新增服务器', 'Add server')}" aria-label="${az('新增服务器', 'Add server')}"`,
      )
  } ${
    ap.step === 'preview'
      ? ''
      : ab(
        icon('edit'),
        'server-edit:' + serverId,
        `class="icon-button" title="${az('编辑服务器', 'Edit server')}" aria-label="${
          az('编辑服务器', 'Edit server')
        }" ${server ? '' : 'disabled'}`,
      )
  }</div>`;
  if (!server) {
    body += `<p>${az('请先新增服务器。', 'Add a server first.')}</p>`;
  } else if (ap.step === 'instances') {
    body += `<div class="row"><strong>${
      az(
        '显示在资源树中的实例与数据库',
        'Instances and databases shown in the resource tree',
      )
    }</strong><span class="spacer"></span><button class="icon-button" data-archery="refresh-instances" title="${
      az('刷新实例', 'Refresh instances')
    }" aria-label="${az('刷新实例', 'Refresh instances')}">${icon('repeat')}</button></div><p class="muted small">${
      refreshedServers.has(serverId)
        ? az('示例实例已刷新', 'Example instances refreshed')
        : az(
          '尚未刷新；已保存项为本地配置',
          'Not refreshed; saved items are local configuration',
        )
    }</p>`;
    body += managementInstances().map(item => {
      const existing = managementBaseline.find(c => c.instance === item.id),
        checked = ap.selectedInstances.has(item.id);
      return `<details class="archery-group" ${
        managementFocus === item.id ? 'open' : ''
      }><summary><label><input type="checkbox" data-ai="${item.id}" ${checked ? 'checked' : ''}> ${
        h(item.name)
      } <span class="tag">${item.provider.toUpperCase()}</span> ${
        existing ? az('（本地配置）', '(local configuration)') : ''
      }</label></summary>${
        item.unavailable
          ? `<p class="field-error">${
            az(
              '平台未返回此实例；已保留本地配置，可明确取消勾选删除。',
              'The platform did not return this instance. Local configuration is retained; deselect explicitly to delete it.',
            )
          }</p>`
          : ''
      }${
        checked
          ? `<label>${
            az('连接名称', 'Connection name')
          } <span class="required-mark" aria-hidden="true">*</span><input data-connection-name="${item.id}" value="${
            h(connectionNames[item.id])
          }" required aria-required="true"></label><p class="field-caption">${
            az(
              '数据库范围与默认数据库',
              'Database scope and default database',
            )
          } <span class="required-mark" aria-hidden="true">*</span></p>${
            item.dbs.map(db =>
              `<div class="archery-actions"><label><input type="checkbox" data-ad="${item.id}:${h(db)}" ${
                ap.selected[item.id].includes(db) ? 'checked' : ''
              } ${item.unavailable ? 'disabled' : ''}> ${
                h(db)
              }</label>${`<label class="archery-default"><input type="radio" name="default-${
                h(item.id)
              }" data-default-instance="${h(item.id)}" value="${h(db)}" ${
                ap.defaults[item.id] === db ? 'checked' : ''
              } ${!item.unavailable && ap.selected[item.id].includes(db) ? '' : 'disabled'}> ${
                az('默认数据库', 'Default database')
              }</label>${
                !item.unavailable && !ap.selected[item.id].includes(db)
                  ? `<span class="muted small">${az('请先选择此数据库', 'Select this database first')}</span>`
                  : ''
              }`}</div>`
            ).join('')
          }`
          : existing
          ? `<p>${
            az(
              '待删除本地连接；勾回可恢复原值。',
              'Local connection pending deletion; select again to restore its original values.',
            )
          }</p>`
          : ''
      }</details>`;
    }).join('');
  } else {
    const describe = c =>
      `${h(c.name)} · ${h(c.instanceName)} · ${c.dbs.map(h).join(', ')} · ${az('默认', 'Default')}: ${h(c.defaultDb)}`;
    for (
      const [label, list, kind] of [
        [az('新增', 'Added'), changes.added, 'add'],
        [az('修改', 'Modified'), changes.modified, 'modify'],
        [az('删除', 'Deleted'), changes.removed, 'delete'],
      ]
    ) {
      if (!list.length) continue;
      body += `<div class="archery-group ${
        kind === 'delete' && list.length ? 'management-delete' : ''
      }"><strong>${label} (${list.length})</strong>${
        list.map(c =>
          `<p>${
            kind === 'modify'
              ? `${az('原', 'Before')}: ${
                describe(
                  managementBaseline.find(b => b.instance === c.instance),
                )
              }<br>${az('新', 'After')}: `
              : ''
          }${describe(c)}</p>`
        ).join('') || ' —'
      }</div>`;
    }
    body += `<p class="muted small">${
      az(
        '删除仅影响本地连接配置，服务器、远端数据与历史保留。存在任务、草稿或占用时整批拒绝。',
        'Deletion affects local connection configuration only; the server, remote data and history remain. Tasks, drafts or other use block the entire batch.',
      )
    }</p>`;
  }
  if (authExampleState === 'archery-save-unknown') {
    body += `<p role="status">${
      az(
        '保存结果未知；示例未发送请求。请重新核对本地连接配置，不要自动重试。',
        'Save outcome unknown; this example sent no request. Recheck local connections before retrying.',
      )
    }</p>`;
    if (recoveryReviewed) {
      body += `<div role="status"><p>${
        az(
          '本地示例连接如下；未读取远端或确认保存结果，草稿保留。',
          'Local example connections are shown below; no remote result was read or confirmed. The draft is retained.',
        )
      }</p><ul>${
        archeryConnections.filter(c => c.server === serverId).map(c =>
          `<li>${h(c.name)} · ${h(c.instanceName)} · ${h(c.dbs.join(', '))}</li>`
        ).join('') || `<li>${az('无连接', 'No connections')}</li>`
      }</ul></div>`;
    }
  }
  const footer = `${ab(az('取消', 'Cancel'), 'close')}<span class="spacer"></span>${
    ap.step === 'instances'
      ? ab(
        az('预览连接', 'Preview connections'),
        'preview',
        'class="primary"',
      )
      : `${authExampleState === 'archery-save-unknown' ? ab(az('核对结果', 'Review result'), 'reconcile-save') : ''} ${
        ab(az('返回修改', 'Back to edit'), 'back')
      } ${
        ab(
          az('保存全部连接', 'Save all connections'),
          'save',
          authExampleState === 'archery-save-unknown' ? 'class="primary" disabled' : 'class="primary"',
        )
      }`
  }`;
  return modal(
    az('服务器连接管理', 'Manage server connections'),
    body,
    footer,
  );
}
function archeryDialogs() {
  if (serverForm && !authStage) {
    const f = serverForm,
      https = f.url.toLowerCase().startsWith('https://');
    const input = (key, label, type = 'text', readonly = false) => {
      const required = key !== 'password'
        || (!f.editing && !f.passwordSaved);
      return `<label>${label}${
        required ? ' <span style="color:var(--danger,red)">*</span>' : ''
      }<input data-server-field="${key}" type="${type}" value="${h(f[key] || '')}" ${readonly ? 'readonly' : ''} ${
        required ? 'required aria-required="true"' : ''
      } autocomplete="off"></label>`;
    };
    const body = `<p class="archery-note">${
      az(
        '仅本地示例，不发送请求或保存凭据。',
        'Local example only; no requests or credentials are saved.',
      )
    }</p><div class="fields">${input('name', az('服务器名称', 'Server name'))}${
      input('url', az('服务器地址', 'Server URL'), 'url', f.editing)
    }${input('username', az('用户名', 'Username'), 'text', f.editing)}${
      f.editing ? '' : input('password', az('密码', 'Password'), 'password')
    }</div>
          ${f.editing ? ab(az('重新登录或修改密码', 'Sign in again or change password'), 'server-reauth') : ''}
          <label class="connection-switch"><span>${
      az('保存密码', 'Save password')
    }</span><input type="checkbox" role="switch" data-save-password ${f.savePassword ? 'checked' : ''}></label>
          ${
      f.editing && f.passwordSaved
        ? `<label class="connection-switch"><span>${
          az('清除已保存密码', 'Clear saved password')
        }</span><input type="checkbox" role="switch" data-clear-password ${f.clearPassword ? 'checked' : ''}></label>`
        : ''
    }
          <p class="muted small">${
      az(
        '保存后地址和用户名不可修改；更换账号请新建服务器。运行任务期间不可保存认证修改。',
        'URL and username are read-only after saving; create another server to change accounts. Authentication changes cannot be saved while tasks run.',
      )
    }</p>
          ${
      f.editing
        ? `<p class="muted small">${
          az(
            `此配置由 ${
              archeryConnections.filter(c => c.server === f.id).length
            } 个连接共享；修改认证配置将作用于关联连接。`,
            `This configuration is shared by ${
              archeryConnections.filter(c => c.server === f.id).length
            } connections; authentication changes apply to them.`,
          )
        }</p>`
        : ''
    }
          <details><summary data-ca-summary aria-disabled="${!https}">${az('CA 证书', 'CA certificate')} · ${
      https
        ? (f.ca
          ? h(f.ca.split('/').pop())
          : az('默认信任链', 'Default trust chain'))
        : az('仅 HTTPS 使用', 'HTTPS only')
    }</summary><label>${az('CA 证书路径', 'CA certificate path')}<input data-server-field="ca" value="${h(f.ca)}" ${
      https ? '' : 'disabled'
    }></label>${ab(az('清除 CA', 'Clear CA'), 'ca-clear', https ? '' : 'disabled')}<p class="muted small">${
      az('仅 HTTPS 使用 CA 证书。', 'CA certificates apply to HTTPS only.')
    }</p></details>
          <details><summary>TOTP · ${
      f.totpMode === 'automatic'
        ? az('自动生成验证码', 'Generate verification codes')
        : az('手动输入', 'Manual entry')
    }</summary><label>${az('验证码方式', 'Verification code method')}<select data-totp-mode><option value="manual" ${
      f.totpMode !== 'automatic' ? 'selected' : ''
    }>${az('手动输入', 'Manual entry')}</option><option value="automatic" ${
      f.totpMode === 'automatic' ? 'selected' : ''
    }>${az('TOTP 链接自动生成', 'Generate from a TOTP URI')}</option></select></label>
          <p class="muted small">${
      f.totp && !f.clearTotp
        ? az(
          '已配置种子，留空保留；原链接不回填。',
          'A seed is configured. Leave blank to keep it; the original URI is never displayed.',
        )
        : az('未配置种子。', 'No seed configured.')
    }</p><p>${
      az(
        '不会启用平台双重认证或绑定二维码。',
        'This does not enable platform 2FA or bind a QR code.',
      )
    }</p><label>${
      az(
        '配置或替换 otpauth://totp/ 链接',
        'Configure or replace an otpauth://totp/ URI',
      )
    }${
      f.totpMode === 'automatic' && (!f.totp || f.clearTotp)
        ? ' <span style="color:var(--danger,red)">*</span>'
        : ''
    }<input data-server-field="totpInput" type="password" value="${h(f.totpInput || '')}" autocomplete="off" ${
      f.totpMode === 'automatic'
        ? ((!f.totp || f.clearTotp) ? 'required aria-required="true"' : '')
        : 'disabled'
    }></label><label class="connection-switch"><span>${
      az('保存 TOTP 种子', 'Save TOTP seed')
    }</span><input data-save-totp type="checkbox" role="switch" ${
      f.saveTotp ? 'checked' : ''
    }></label><label class="connection-switch"><span>${
      az('清除已保存种子', 'Clear saved seed')
    }</span><input data-server-clear type="checkbox" role="switch" ${f.clearTotp ? 'checked' : ''}></label></details>
          <p class="muted small">${
      az(
        '密码与种子分别保存在系统凭据库；不可用时仅保留在 Engine 会话。此示例仅展示状态，不保存或生成验证码。',
        'Password and seed are stored separately in the system credential store; if unavailable they remain only in the Engine session. This example only shows state and never stores or generates codes.',
      )
    }</p>`;
    return modal(
      f.editing
        ? az('编辑服务器', 'Edit server')
        : az('新增服务器', 'Add server'),
      body,
      `${ab(az('取消', 'Cancel'), 'server-cancel')} <span class="spacer"></span>${
        ab(az('保存', 'Save'), 'server-save', 'class="primary"')
      }`,
    );
  }
  if (connectionDelete) {
    const c = archeryConnections.find(item =>
      item.server === connectionDelete.server
      && item.instance === connectionDelete.instance
    );
    return modal(
      az('删除连接', 'Delete connection'),
      `<p>${h(c?.name || '')}</p><p>${
        az(
          '仅删除本地连接配置。',
          'Only the local connection configuration is deleted.',
        )
      }</p>`,
      `${ab(az('取消', 'Cancel'), 'connection-delete-cancel')} <span class="spacer"></span>${
        ab(
          az('删除连接', 'Delete connection'),
          'connection-delete-confirm',
          'class="primary server-danger"',
        )
      }`,
    );
  }
  if (serverDelete) {
    const s = serverDeleteSnapshot?.server,
      linked = serverDeleteSnapshot?.connections || [];
    const deleting = authExampleState === 'archery-delete-busy';
    const deleteFailed = authExampleState === 'archery-delete-failed';
    const deleteUnknown = authExampleState === 'archery-delete-unknown';
    if (!s) return '';
    return modal(
      az('删除服务器', 'Delete server'),
      `<p class="archery-note">${
        az('交互示例：不会发送删除请求。', 'Interactive example: no deletion request is sent.')
      }</p><p>${h(s.name)} · ${h(s.url)} · ${h(s.username)}</p><p>${az('关联连接', 'Associated connections')}: ${
        linked.map(c => h(c.name)).join(', ') || '—'
      }</p><p>${
        az(
          '仅删除本地配置，不删除远端数据；历史保留。运行任务、草稿或其他占用会阻止删除。',
          'Deletes local configuration only, not remote data; history is retained. Running tasks, drafts or other use block deletion.',
        )
      }</p>${
        deleting
          ? `<p role="status">${
            az(
              '删除中（状态示例）；停止等待不会取消远端操作。',
              'Deleting (example state); stopping the wait does not cancel a remote operation.',
            )
          }</p>`
          : ''
      }${
        deleteFailed
          ? `<p role="alert">${
            az(
              '删除失败（状态示例）；目标保留，可重新核对。',
              'Deletion failed (example state); the target is retained for review.',
            )
          }</p>`
          : ''
      }${
        deleteUnknown
          ? `<p role="status">${
            az(
              '删除结果未知；请重新核对服务器与连接，勿自动重试。',
              'Deletion outcome unknown; recheck the server and connections before retrying.',
            )
          }</p>`
          : ''
      }${
        deleteUnknown && recoveryReviewed
          ? `<p role="status">${
            az(
              `本地示例中服务器${
                servers.some(item => item.id === serverDelete) ? '仍存在' : '不存在'
              }；未读取远端或确认删除结果。`,
              `The server ${
                servers.some(item => item.id === serverDelete) ? 'is present' : 'is absent'
              } in the local example; no remote deletion result was read or confirmed.`,
            )
          }</p>`
          : ''
      }`,
      `${ab(az(deleting ? '停止等待' : '取消', deleting ? 'Stop waiting' : 'Cancel'), 'server-delete-cancel')} ${
        deleteUnknown ? ab(az('核对结果', 'Review result'), 'reconcile-delete') : ''
      } <span class="spacer"></span>${
        ab(
          az(
            `删除服务器及 ${linked.length} 个连接`,
            `Delete server and ${linked.length} connections`,
          ),
          'server-delete-confirm',
          deleting || deleteUnknown ? 'class="primary server-danger" disabled' : 'class="primary server-danger"',
        )
      }`,
    );
  }
  if (authStage) {
    const s = serverForm || servers.find(item => item.id === serverId);
    const authBusy = authExampleState === 'archery-auth-busy';
    const authObserve = authExampleState === 'archery-auth-observe';
    const authFailed = authExampleState === 'archery-auth-failed';
    const authUnknown = authExampleState === 'archery-auth-unknown';
    return modal(
      az('服务器认证', 'Server authentication'),
      `<p>${h(s?.name || '')} · ${h(s?.username || '')}</p><p class="archery-note">${
        az(
          '独立认证示例；不会发送请求或保存凭据。',
          'Independent authentication example; no request or credentials are saved.',
        )
      }</p>${
        authBusy
          ? `<p role="status">${
            az(
              '认证中（状态示例）；停止等待不会取消远端操作。',
              'Authenticating (example state); stopping the wait does not cancel a remote operation.',
            )
          }</p>`
          : ''
      }${
        authObserve
          ? `<p role="status">${
            az(
              '其他窗口正在处理认证；此处只能观察。',
              'Authentication is controlled by another window; this window can only observe.',
            )
          }</p>`
          : ''
      }${
        authUnknown
          ? `<p role="status">${
            az(
              '已停止等待，远端状态未知；请重新核对认证状态。',
              'Stopped waiting; the remote state is unknown. Recheck authentication status.',
            )
          }</p>`
          : ''
      }${
        authUnknown && recoveryReviewed
          ? `<p role="status">${
            az(
              '本地示例无法读取或确认远端认证状态；未发送请求，可返回后人工核对。',
              'The local example cannot read or confirm remote authentication status; no request was sent. Return and check it separately.',
            )
          }</p>`
          : ''
      }${
        authFailed
          ? `<p role="alert">${
            az(
              '认证失败（状态示例）；输入保留，可重试。',
              'Authentication failed (example state); input is retained and can be retried.',
            )
          }</p>`
          : ''
      }<label>${
        authStage === 'password'
          ? az('密码', 'Password')
          : az('验证码', 'Verification code')
      } <span style="color:var(--danger,red)">*</span><input type="${
        authStage === 'password' ? 'password' : 'text'
      }" data-auth-input required aria-required="true" autocomplete="${
        authStage === 'password' ? 'off' : 'one-time-code'
      }" ${authBusy || authObserve || authUnknown ? 'disabled' : ''} autofocus></label>`,
      `${ab(az(authBusy ? '停止等待' : '取消', authBusy ? 'Stop waiting' : 'Cancel'), 'auth-cancel')} ${
        authUnknown ? ab(az('核对认证状态', 'Review authentication status'), 'reconcile-auth') : ''
      } ${
        ab(
          authStage === 'password'
            ? az('继续', 'Continue')
            : az('验证', 'Verify'),
          'auth-next',
          authBusy || authObserve || authUnknown ? 'class="primary" disabled' : 'class="primary"',
        )
      }`,
    );
  }
  return '';
}

'use strict';
for (const type of ['input', 'change']) {
  document.addEventListener(type, event => {
    if (state.overlay !== 'import' || !event.target.closest('#app .modal-body')) return;
    document.querySelector('[data-action="request-preview-import"]')?.classList.replace('button', 'primary');
    document.querySelector('[data-action="request-import"]')?.classList.replace('primary', 'button');
  });
}
function draftOverlay() {
  const parameterTable = rows =>
    `<table class="preview-bindings"><thead><tr>${
      [
        'parameterNumber',
        'parameterType',
        'parameterBytes',
        'parameterValue',
      ].map(key => `<th>${h(t(key))}</th>`).join('')
    }</tr></thead><tbody>${
      rows.map(([number, type, bytes, value, dynamic]) =>
        `<tr><td>${number}</td><td>${h(type)}</td><td>${bytes}</td><td>${h(value)} <span class="muted">· ${
          h(t(dynamic ? 'dynamicValue' : 'completeValue'))
        }</span></td></tr>`
      ).join('')
    }</tbody></table>`;
  return modal(
    t('changePreview'),
    `<p class="muted small">${h(t('previewExample'))} · ${h(t('sample'))}</p>${
      state.provider === 'mysql'
        ? `<strong>CHECK</strong><pre class="preview-code">${
          h('SELECT 1 FROM `example`.`orders` WHERE BINARY `id` <=> BINARY ? AND BINARY `status` <=> BINARY ? FOR UPDATE')
        }</pre>
              <strong>WRITE</strong><pre class="preview-code">${
          h('UPDATE `example`.`orders` SET `status` = ? WHERE BINARY `id` <=> BINARY ?')
        }</pre><strong>CHECK · ${h(t('bindings'))}</strong>${
          parameterTable([[1, 'text', 4, '1042'], [
            2,
            'text',
            7,
            'pending',
          ]])
        }<strong>WRITE · ${h(t('bindings'))}</strong>${
          parameterTable([[1, 'text', 4, 'paid'], [2, 'text', 4, '1042']])
        }<p class="muted small">${h(t('validationRole'))}</p>`
        : `<p class="muted small">${
          h(
            language === 'zh-CN'
              ? '示例草稿；未发送 Redis 命令。批量删除计划为 WATCH 全部目标，MULTI、一次 DEL 全部目标、EXEC；实际计划与参数由 Engine 生成。'
              : 'Sample drafts; no Redis command was sent. Batch deletion plans WATCH for all targets, then MULTI, one DEL for all targets, and EXEC. Engine generates the actual plan and parameters.',
          )
        }</p><strong>${az('命令示例', 'Sample command')}</strong><pre class="preview-code">HSET ? ? ?</pre>
        <strong>${h(t('bindings'))}</strong>${
          parameterTable([[1, 'text', 9, 'user:1042'], [2, 'text', 4, 'plan'], [3, 'text', 3, 'pro']])
        }
        <p class="muted small">${
          az(
            '示例参数完整；动态值将在生成实际计划时确定。',
            'Sample parameters are complete; dynamic values are determined when the actual plan is generated.',
          )
        }</p>
        <details><summary>${az('动态参数示例', 'Dynamic parameter example')}</summary>${
          parameterTable([[1, 'bytes', '—', az('执行时确定', 'Determined at execution'), true]])
        }</details>
        <div class="table-footer"><button class="button" disabled>${h(t('previousPage'))}</button><span>${
          az('示例第 1 / 1 页', 'Sample page 1 / 1')
        }</span><button class="button" disabled>${h(t('nextPage'))}</button></div>
        <details><summary>${az('不可预览状态示例', 'Unavailable preview example')}</summary><p>${
          az(
            '计划尚未生成或参数不可用时，无法预览命令；请保留草稿并重试。',
            'Commands cannot be previewed while the plan or parameters are unavailable; retain the draft and retry.',
          )
        }</p></details>`
    }`,
    `<span class="spacer"></span><button class="button" data-action="close-modal">${
      t('back')
    }</button><button class="primary" data-action="draft-commit">${t('demonstrateSubmit')}</button>`,
  );
}
function formOverlay(kind) {
  const key = keySamples.find(k => k.name === state.key);
  const label = (text, input, required = false, wide = false) =>
    `<label${wide ? ' class="wide"' : ''}>${h(text)}${
      required ? ' <span style="color:var(--danger,red)">*</span>' : ''
    }${input}</label>`;
  const field = (id, value = '', required = false) =>
    `<input id="${id}" value="${h(value)}" ${required ? 'required aria-required="true"' : ''}>`;
  const foot = action =>
    `<span class="spacer"></span><button class="button" data-action="close-modal">${
      t('cancel')
    }</button><button class="${
      action.startsWith('stage-delete') ? 'button danger' : 'primary'
    }" data-action="${action}">${t('stageDraft')}</button>`;
  if (kind === 'draft-preview') return draftOverlay();
  if (kind === 'filter') {
    return modal(
      t('filterSort'),
      `<div class="fields">${
        label(
          t('field'),
          `<select id="filter-column"><option value="">${az('不筛选', 'No filter')}</option>${
            Object.keys(databases[state.database][state.table][0]).map(c => `<option>${h(c)}</option>`).join('')
          }</select>`,
        )
      }${
        label(
          t('condition'),
          `<select id="filter-operator" required aria-required="true">${
            [
              ['eq', az('等于', 'Equals')],
              ['ne', az('不等于', 'Not equal')],
              ['lt', az('小于', 'Less than')],
              ['lte', az('小于等于', 'Less than or equal')],
              ['gt', az('大于', 'Greater than')],
              ['gte', az('大于等于', 'Greater than or equal')],
              ['like', 'LIKE'],
              ['not_like', 'NOT LIKE'],
              ['is_null', az('是 NULL', 'Is NULL')],
              ['is_not_null', az('不是 NULL', 'Is not NULL')],
            ].map(([value, text]) => `<option value="${value}">${h(text)}</option>`).join('')
          }</select>`,
          true,
        )
      }${label(t('value'), field('filter-value'))}${
        label(
          t('sortField'),
          `<select id="sort-column"><option value="">${h(t('sortNone'))}</option>${
            Object.keys(databases[state.database][state.table][0]).map(c => `<option value="${h(c)}">${h(c)}</option>`)
              .join('')
          }</select>`,
        )
      }${
        label(
          az('排序方向', 'Sort direction'),
          `<select id="sort-direction"><option value="asc">${h(t('ascending'))}</option><option value="desc">${
            h(t('descending'))
          }</option></select>`,
        )
      }</div><details><summary>${az('高级多条件 JSON', 'Advanced conditions JSON')}</summary><label>${
        az('过滤 JSON（支持多条件）', 'Filter JSON (multiple conditions)')
      }<textarea id="filter-advanced-json" rows="6" spellcheck="false" placeholder="[]"></textarea></label><label>${
        az('排序 JSON', 'Sort JSON')
      }<textarea id="sort-advanced-json" rows="4" spellcheck="false" placeholder="[]"></textarea></label><p class="muted small">${
        az('无效 JSON 会在字段旁提示。', 'Invalid JSON is reported beside the field.')
      }</p></details>`,
      `<button class="button" data-action="clear-filter">${
        t('clearFilter')
      }</button><span class="spacer"></span><button class="primary" data-action="apply-filter">${
        t('applyFilter')
      }</button>`,
    );
  }
  if (kind === 'new-row') {
    return modal(
      t('newRow'),
      `<p class="muted small">${
        az(
          '字段类型与生成列仅为设计示例，实际表单以表结构为准。',
          'Column types and generated columns are illustrative; the actual form uses the table schema.',
        )
      }</p><div class="fields">${
        schemaInitialColumns({ database: state.database, table: state.table }).map(column =>
          `<div data-new-column="${h(column.name)}" data-new-nullable="${column.nullable}"><strong>${
            h(column.name)
          } · ${h(column.type)}</strong><label>${
            az('字段模式', 'Field mode')
          }<select data-new-mode><option value="default">${az('默认', 'Default')}</option><option value="value">${
            az('输入值', 'Value')
          }</option>${column.nullable ? '<option value="null">NULL</option>' : ''}</select></label><label>${
            az('值', 'Value')
          }<input id="new-${h(column.name)}" data-new-value disabled></label></div>`
        ).join('')
      }</div><p class="muted small">${
        az(
          '默认由数据库填充；输入值可为空字符串，NULL 与空字符串不同。生成列由数据库生成，不可输入。',
          'Defaults are supplied by the database; a value may be an empty string, which differs from NULL. Generated columns are supplied by the database and cannot be edited.',
        )
      }</p><div class="fields"><label>${
        az('生成列示例 · 类型见表结构', 'Generated column example · type from schema')
      }<input disabled value="${az('由数据库生成', 'Generated by database')}"></label></div><details><summary>${
        az('高级 JSON', 'Advanced JSON')
      }</summary><label>${
        az('类型化行 JSON', 'Typed row JSON')
      }<textarea id="new-row-json" rows="6" spellcheck="false" placeholder="{}"></textarea></label><p class="muted small">${
        az('无效 JSON 会在字段旁提示。', 'Invalid JSON is reported beside the field.')
      }</p></details><p class="muted small">${t('formPreviewOnlyNoDatabaseWrites')}</p>`,
      foot('stage-new-row'),
    );
  }
  if (kind === 'export' || kind === 'import' || kind === 'transfer') {
    return modal(
      t(
        kind === 'export'
          ? 'exportData'
          : kind === 'import'
          ? 'importData'
          : 'transferTitle',
      ),
      `<div class="fields"><label class="wide">${
        h(t('filePath'))
      } <span style="color:var(--danger,red)">*</span><input id="transfer-path" value="orders.csv" required aria-required="true">${''}</label><label>${
        h(t('format'))
      } <span class="required-mark" aria-hidden="true">*</span><select id="transfer-format" required aria-required="true"><option>CSV</option><option>JSON</option></select></label><details class="wide"><summary>${
        h(t('transferAdvanced'))
      }</summary><div class="fields"><label class="wide">${
        h(t('mappingJSON'))
      } <span style="color:var(--danger,red)">*</span><textarea id="transfer-mapping" rows="5" required aria-required="true" spellcheck="false">{}</textarea></label><label>${
        h(t('nullMarker'))
      }${field('transfer-null', '\\N')}</label><label class="connection-switch"><span>${
        h(t('backslashEscape'))
      }</span><input id="transfer-escape" type="checkbox" role="switch" checked></label><p class="muted small">${
        az(
          '开关修改表单草稿，在预览或提交当前请求时使用。',
          'The switch edits the form draft and is used when previewing or submitting this request.',
        )
      }</p><label class="wide">${
        h(t('binaryColumns'))
      } <span style="color:var(--danger,red)">*</span><textarea id="transfer-binary" rows="5" required aria-required="true" spellcheck="false">[]</textarea></label></div></details></div>${
        kind === 'import' ? '' : `<p class="muted small">${h(t('exportScope'))}</p>`
      }${
        kind !== 'export' && state.connectionModes[state.provider]
          ? `<p class="notice">${
            az(
              '当前连接为只读，无法预览或导入数据。',
              'The current connection is read-only; import and its preview are unavailable.',
            )
          }</p>`
          : ''
      }<details><summary>${az('受限状态示例', 'Restricted state examples')}</summary><p>${
        az(
          'Archery 不支持导入；支持导出已缓存结果，结果尚未完整时不能导出完整结果。',
          'Archery does not support import; cached results can be exported, but complete-result export is unavailable while results are incomplete.',
        )
      }</p><button class="button" disabled>${
        az('导出完整结果', 'Export complete result')
      }</button></details><p class="muted small">${
        h(t('requestPreviewOnly'))
      }</p><p id="request-preview" class="notice" hidden></p>`,
      `<span class="spacer"></span>${
        kind === 'export'
          ? ''
          : `<button class="${kind === 'import' ? 'primary' : 'button'}" data-action="request-preview-import" ${
            state.connectionModes[state.provider] ? 'disabled' : ''
          }>${h(t('previewImport'))}</button><button class="button" data-action="request-import" ${
            state.connectionModes[state.provider] ? 'disabled' : ''
          }>${h(t('confirmImport'))}</button>`
      }${
        kind === 'import'
          ? ''
          : `<button class="primary" data-action="request-export">${h(t('previewExport'))}</button>`
      }`,
    );
  }
  if (kind === 'query-file') {
    return modal(
      t('queryFile'),
      `<div class="fields"><label class="wide">${
        h(t('filePath'))
      } <span style="color:var(--danger,red)">*</span><input id="query-file-path" value="orders.sql" required aria-required="true">${''}</label></div><p class="muted small">${
        h(t('requestPreviewOnly'))
      }</p><p id="request-preview" class="notice" hidden></p>`,
      `<span class="spacer"></span><button class="button" data-action="request-file-open">${
        h(t('fileOpen'))
      }</button><button class="primary" data-action="request-file-save">${h(t('fileSave'))}</button>`,
    );
  }
  if (!key) return '';
  if (kind === 'ttl') {
    return modal(
      t('editTtl'),
      `<div class="fields">${
        label(
          t('expiryPolicy'),
          `<select id="redis-ttl-policy" required aria-required="true"><option value="keep">${
            t('keepTTL')
          }</option><option value="set">${t('setDuration')}</option><option value="persist">${
            t('forever')
          }</option></select>`,
          true,
        )
      }<label id="ttl-value-label" hidden>${
        az('持续时间（毫秒）', 'Duration (milliseconds)')
      }<span id="ttl-required" style="color:var(--danger,red)" hidden> *</span><input id="redis-ttl-value" type="number" min="0" step="1" value="3600000" aria-describedby="ttl-hint"><span id="ttl-hint" class="muted small">${
        az(
          '设置策略时必填，输入非负整数毫秒；0 表示立即过期。',
          'Required for the set policy; enter a nonnegative integer in milliseconds. 0 expires immediately.',
        )
      }</span></label></div>`,
      foot('stage-ttl'),
    );
  }
  if (kind === 'rename-key') {
    return modal(
      t('renameKey'),
      `<div class="fields">${label(t('currentKey'), `<input readonly value="${h(key.name)}">`)}${
        label(t('newKey'), field('redis-new-name'))
      }</div>`,
      foot('stage-rename'),
    );
  }
  if (kind === 'delete-key') {
    return modal(
      t('deleteKey'),
      `<p>DB ${state.redisDeleteTarget.db}</p><ul>${
        state.redisDeleteTarget.keys.map(item => `<li>${h(item.name)} · ${h(item.type)}</li>`).join('')
      }</ul><p class="muted">${h(t('deletionIsStagedAndWillNotBeExecuted'))}</p>`,
      foot('stage-delete'),
    );
  }
  if (kind === 'delete-entry') {
    return modal(
      t('deleteEntry'),
      `<p>DB ${state.redisDeleteTarget.db} / ${h(state.redisDeleteTarget.key.name)}</p><ul>${
        state.redisDeleteTarget.entries.map(entry => `<li>${h(entry.row.join(' / '))}</li>`).join('')
      }</ul><p class="muted">${h(t('deletionIsStagedAndWillNotBeExecuted'))}</p>`,
      foot('stage-delete-entry'),
    );
  }
  if (kind === 'edit-key' || kind === 'add-entry') {
    const row = key.rows?.[state.redisRow] || [];
    const draft = currentDrafts().find(d => d.kind === 'edit' && d.row === state.redisRow);
    const values = kind === 'add-entry'
      ? (key.type === 'LIST'
        ? [String(key.rows.length), '']
        : key.type === 'SET'
        ? ['']
        : ['', ''])
      : draft?.after ?? (key.type === 'STR' ? [key.value] : row);
    const names = key.type === 'HASH'
      ? [t('field'), t('value')]
      : key.type === 'LIST'
      ? [t('index'), t('value')]
      : key.type === 'ZSET'
      ? [t('member'), t('score')]
      : [key.type === 'SET' ? t('member') : t('value')];
    return modal(
      t(kind === 'add-entry' ? 'addEntry' : 'editValue'),
      `<p class="muted small">DB ${h(state.redisDb)} / ${h(key.name)} · ${h(key.type)}</p><p class="muted small">${
        az(
          '当前条目身份由上方 key 和下列字段确定。Key、Field、Member 和 Value 允许为空；base64: 前缀可表示空字节。',
          'The key above and fields below identify the entry. Key, field, member and value may be empty; the base64: prefix can represent zero bytes.',
        )
      }</p><div class="fields">${
        names.map((name, i) => {
          const singleLine = (key.type === 'LIST' && i === 0)
            || (key.type === 'HASH' && i === 0)
            || (key.type === 'ZSET' && i === 1);
          return label(
            name,
            singleLine
              ? `<input id="redis-edit-${i}" value="${h(values[i])}" ${
                key.type === 'ZSET' && i === 1
                  ? 'required aria-required="true"'
                  : ''
              } ${key.type === 'LIST' && i === 0 ? 'readonly' : ''}>`
              : `<textarea id="redis-edit-${i}" rows="5">${h(values[i])}</textarea>`,
            key.type === 'ZSET' && i === 1,
            true,
          );
        }).join('')
      }</div><p class="muted small">${t('editAnInMemoryDraftTTLIsRetained')}</p>`,
      foot(kind === 'add-entry' ? 'stage-add-entry' : 'stage-edit'),
    );
  }
  return '';
}

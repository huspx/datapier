'use strict';
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
        }</p>${
          simpleTable(
            [t('resource'), t('operation'), t('oldValue')],
            currentDrafts().map(
              d => [
                d.label,
                d.kind,
                typeof d.before === 'string'
                  ? d.before
                  : JSON.stringify(d.before),
              ],
            ),
          )
        }`
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
          `<select id="filter-column" required aria-required="true">${
            Object.keys(databases[state.database][state.table][0]).map(c => `<option>${h(c)}</option>`).join('')
          }</select>`,
          true,
        )
      }${
        label(
          t('condition'),
          `<select id="filter-operator" required aria-required="true"><option value="contains">${
            t('contains')
          }</option><option value="equals">${t('equals')}</option><option value="null">NULL</option></select>`,
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
          t('condition'),
          `<select id="sort-direction"><option value="asc">${h(t('ascending'))}</option><option value="desc">${
            h(t('descending'))
          }</option></select>`,
        )
      }</div>`,
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
      `<div class="fields">${
        Object.keys(databases[state.database][state.table][0]).map(c => {
          const sample = databases[state.database][state.table][0][c];
          const required = c !== 'id' && c !== 'created_at'
            && (typeof sample === 'number'
              || (sample !== null && typeof sample === 'object'));
          return label(c, field('new-' + c, '', required), required);
        }).join('')
      }</div><p class="muted small">${t('formPreviewOnlyNoDatabaseWrites')}</p>`,
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
      }</summary><div class="fields"><label>${h(t('mappingJSON'))} <span style="color:var(--danger,red)">*</span>${
        field('transfer-mapping', '{}', true)
      }</label><label>${h(t('nullMarker'))}${field('transfer-null', '\\N')}</label><label>${
        h(t('backslashEscape'))
      } <span class="required-mark" aria-hidden="true">*</span><select id="transfer-escape" required aria-required="true"><option>true</option><option>false</option></select></label><label>${
        h(t('binaryColumns'))
      } <span style="color:var(--danger,red)">*</span>${
        field('transfer-binary', '[]', true)
      }</label></div></details></div>${
        kind === 'import' ? '' : `<p class="muted small">${h(t('exportScope'))}</p>`
      }<p class="muted small">${h(t('requestPreviewOnly'))}</p><p id="request-preview" class="notice" hidden></p>`,
      `<span class="spacer"></span>${
        kind === 'export'
          ? ''
          : `<button class="button" data-action="request-preview-import" ${
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
      }${`<label>${
        h(t('durationSeconds'))
      }<span id="ttl-required" style="color:var(--danger,red)" hidden> *</span><input id="redis-ttl-value" value="3600"></label>`}</div>`,
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
      `<div class="fields">${
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

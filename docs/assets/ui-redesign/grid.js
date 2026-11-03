'use strict';
function simpleTable(columns, rows, resultGrid = false) {
  return `<table ${resultGrid ? 'id="result-grid"' : ''}>${
    resultGrid
      ? `<colgroup>${columns.map(c => `<col data-grid-column="${h(c)}">`).join('')}</colgroup>`
      : ''
  }<thead><tr>${columns.map(c => `<th>${h(c)}</th>`).join('')}</tr></thead><tbody>${
    rows.map(row =>
      `<tr>${
        row.map((v, i) =>
          `<td ${resultGrid ? `data-cell="${h(columns[i])}" tabindex="0"` : ''}>${
            v === null ? '<span class="null">NULL</span>' : h(v)
          }</td>`
        ).join('')
      }</tr>`
    ).join('')
  }</tbody></table>${
    resultGrid
      ? `<div class="table-footer"><span class="spacer"></span>${
        ib(
          'copy',
          t('copyCells'),
          'copy-cells',
          state.cellSelection ? '' : 'disabled',
        )
      }</div>`
      : ''
  }`;
}
function gridCell(row, column) {
  if (state.cellEdit?.row === row && state.cellEdit.column === column) {
    return `<input id="grid-cell-editor" class="cell-editor" value="${h(state.cellEdit.value)}" aria-label="${
      h(t('editCell') + ': ' + column)
    }" title="${h(t('cellEditHelp'))}">`;
  }
  const value = row[column];
  if (value === null) return '<span class="null">NULL</span>';
  // 数据值按普通文本显示，不按业务字段名推断状态颜色。
  return h(value);
}
function stageCellValue(row, column, value) {
  if (deletedRows.has(row) || value === row[column]) return;
  let originals = cellOriginals.get(row);
  if (!originals) cellOriginals.set(row, originals = new Map());
  if (!originals.has(column)) originals.set(column, row[column]);
  row[column] = value;
  if (value === originals.get(column)) originals.delete(column);
}
function finishCellEdit(keep) {
  const edit = state.cellEdit;
  if (!edit) return;
  const input = $('#grid-cell-editor'), cell = input?.closest('td');
  const value = input?.value ?? edit.value;
  state.cellEdit = null;
  if (keep && value !== String(edit.row[edit.column] ?? '')) {
    stageCellValue(edit.row, edit.column, value);
    toast(t('cellDraftSaved'));
  }
  if (cell) {
    cell.classList.remove('editing');
    cell.classList.toggle(
      'modified',
      !!cellOriginals.get(edit.row)?.has(edit.column),
    );
    cell.innerHTML = gridCell(edit.row, edit.column);
  }
  refreshDraftBar();
  gridMeasureCache = null;
  requestAnimationFrame(fitGrid);
}
function startCellEdit(id, column) {
  finishCellEdit(true);
  const row = databases[state.database]?.[state.table]?.find(row => rowId(row) === id);
  const element = [...document.querySelectorAll('#mysql-grid tbody tr')]
    .find(row => row.dataset.row === id);
  const cell = element?.querySelector(
    `[data-cell="${CSS.escape(column)}"]`,
  );
  if (!row || !cell || deletedRows.has(row)) return;
  state.menu = null;
  document.querySelector('.context-menu')?.remove();
  state.cellEdit = { row, column, value: String(row[column] ?? '') };
  cell.classList.add('editing');
  cell.innerHTML = gridCell(row, column);
  $('#grid-cell-editor').focus();
  $('#grid-cell-editor').select();
}
let gridMeasureCache = null;
function fitGrid() {
  const table = $('#mysql-grid') || $('#result-grid');
  if (!table || state.cellEdit) return;
  const result = table.id === 'result-grid';
  const columns = [...table.querySelectorAll('col[data-grid-column]')];
  const rows = result
    ? orders.filter(row =>
      row.status === 'paid'
      && Object.values(row).some(v => String(v).toLowerCase().includes(state.resultFilter.toLowerCase()))
    )
    : currentRows();
  const key = result ? 'result' : `${state.database}.${state.table}`;
  const numberWidth = result ? 0 : 38;
  const signature = JSON.stringify([
    key,
    columns.map(c => c.dataset.gridColumn),
    rows.map(row => columns.map(col => row[col.dataset.gridColumn])),
  ]);
  if (gridMeasureCache?.signature !== signature) {
    const units = text =>
      [...String(text)].slice(0, 66).reduce(
        (n, c) => n + (c.codePointAt(0) < 128 ? 1 : 2),
        0,
      );
    const headers = [...table.querySelectorAll('thead th')].slice(
      result ? 0 : 1,
    );
    gridMeasureCache = {
      signature,
      widths: columns.map((col, i) => {
        const name = col.dataset.gridColumn;
        const type = headers[i]?.querySelector('small')?.textContent || '';
        return Math.min(
          480,
          Math.max(
            64,
            units(name) * 7 + 24,
            units(type) * 5 + 24,
            ...rows.map(row =>
              Math.min(66, row[name] == null ? 5 : units(row[name])) * 7
              + 24
            ),
          ),
        );
      }),
    };
  }
  const widths = gridMeasureCache.widths.map((width, i) =>
    state.columnWidths[key]?.[columns[i].dataset.gridColumn] ?? width
  );
  const flexible = widths.map((w, i) => i).filter(i =>
    state.columnWidths[key]?.[columns[i].dataset.gridColumn] == null
  );
  let remaining = Math.max(
    flexible.reduce((n, i) => n + widths[i], 0),
    table.parentElement.clientWidth - numberWidth
      - widths.reduce((n, w, i) => n + (flexible.includes(i) ? 0 : w), 0),
  );
  let weight = flexible.reduce((n, i) => n + widths[i], 0);
  let pending = [...flexible];
  while (pending.length) {
    const capped = pending.filter(i => remaining * widths[i] >= 480 * weight);
    if (!capped.length) break;
    for (const i of capped) {
      weight -= widths[i];
      widths[i] = 480;
      remaining -= 480;
    }
    pending = pending.filter(i => !capped.includes(i));
  }
  pending.forEach((i, position) => {
    const natural = widths[i];
    const width = position === pending.length - 1
      ? remaining
      : Math.floor(remaining * natural / weight);
    widths[i] = Math.min(480, Math.max(64, width));
    remaining -= widths[i];
    weight -= natural;
  });
  columns.forEach((col, i) => col.style.width = widths[i] + 'px');
  table.style.width = (numberWidth + widths.reduce((a, b) => a + b, 0))
    + 'px';
}
function paintCellSelection() {
  const selection = state.cellSelection;
  const table = document.querySelector(
    selection?.key.startsWith('redis:')
      ? '#redis-grid'
      : selection?.key.startsWith('result:')
      ? '#result-grid'
      : '#mysql-grid',
  );
  const key = table?.id === 'redis-grid'
    ? `redis:${state.redisDb}:${state.key}`
    : table?.id === 'result-grid'
    ? `result:${state.active}`
    : `${state.database}.${state.table}`;
  const rows = [...(table?.querySelectorAll('tbody tr') || [])];
  rows.forEach((row, r) =>
    [...row.querySelectorAll('td[data-cell]')].forEach((cell, c) => {
      cell.classList.toggle(
        'cell-selected',
        !!selection && selection.key === key
          && r >= Math.min(selection.start[0], selection.end[0])
          && r <= Math.max(selection.start[0], selection.end[0])
          && c >= Math.min(selection.start[1], selection.end[1])
          && c <= Math.max(selection.start[1], selection.end[1]),
      );
    })
  );
  document.querySelectorAll('[data-action="copy-cells"]').forEach(
    button => {
      button.disabled = !selection || selection.key !== key;
    },
  );
}
function copyCellSelection(
  selectedCell = document.querySelector('td.cell-selected[data-cell]'),
) {
  if (!selectedCell || !state.cellSelection) return;
  const s = state.cellSelection;
  const redis = selectedCell.closest('#redis-grid'),
    queryResult = selectedCell.closest('#result-grid');
  const key = redis
    ? `redis:${state.redisDb}:${state.key}`
    : queryResult
    ? `result:${state.active}`
    : `${state.database}.${state.table}`;
  if (s.key !== key) return;
  const rows = [
    ...selectedCell.closest('table').querySelectorAll('tbody tr'),
  ];
  const source = redis
    ? keySamples.find(item => item.name === state.key)?.rows
    : queryResult
    ? orders.filter(row =>
      row.status === 'paid'
      && Object.values(row).some(v => String(v).toLowerCase().includes(state.resultFilter.toLowerCase()))
    )
    : currentRows();
  if (!source) return;
  const result = [];
  for (
    let r = Math.min(s.start[0], s.end[0]);
    r <= Math.max(s.start[0], s.end[0]);
    r++
  ) {
    const cells = [...(rows[r]?.querySelectorAll('td[data-cell]') || [])];
    const fields = [];
    for (
      let c = Math.min(s.start[1], s.end[1]);
      c <= Math.max(s.start[1], s.end[1]);
      c++
    ) {
      if (!cells[c] || !source[r]) return;
      const value = source[r][cells[c].dataset.cell];
      if (value === undefined) return;
      const redisType = redis
        && keySamples.find(item => item.name === state.key)?.type;
      const column = Number(cells[c].dataset.cell);
      const bytes = redis && !(redisType === 'LIST' && column === 0)
        && !(redisType === 'ZSET' && column === 1);
      const encoded = bytes
        ? btoa(
          Array.from(
            new TextEncoder().encode(String(value)),
            byte => String.fromCharCode(byte),
          ).join(''),
        )
        : value;
      let text = encoded === null
        ? '\\N'
        : String(encoded).replaceAll('\\', '\\\\');
      if (/[\t\r\n"]/.test(text)) {
        text = '"' + text.replaceAll('"', '""') + '"';
      }
      fields.push(text);
    }
    result.push(fields.join('\t'));
  }
  copy(result.join('\n') + '\n');
}
function gridPosition(cell) {
  const row = cell.closest('tr');
  return [
    [...row.parentElement.children].indexOf(row),
    [...row.querySelectorAll('td[data-cell]')].indexOf(cell),
  ];
}
function grid() {
  const rows = currentRows();
  const columns = Object.keys(
    databases[state.database]?.[state.table]?.[0] || {},
  ).filter(c => !state.hiddenColumns[`${state.database}.${state.table}`]?.includes(c));
  return `<div class="table-wrap"><table id="mysql-grid"><colgroup><col style="width:38px">${
    columns.map(c => `<col data-grid-column="${h(c)}">`).join('')
  }</colgroup><thead><tr><th class="row-number">#</th>${
    columns.map(c =>
      `<th>${h(c)}<small>${
        c === 'id' || c.endsWith('_id')
          ? 'BIGINT'
          : ['total', 'revenue', 'price'].includes(c)
          ? 'DECIMAL'
          : c === 'created_at'
          ? 'DATETIME'
          : 'VARCHAR'
      }</small></th>`
    ).join('')
  }</tr></thead><tbody>${
    rows.map((row, index) => {
      const identity = rowId(row);
      return `<tr data-row="${h(identity)}" class="${
        state.selected.has(identity) ? 'selected' : ''
      }"><td class="row-number" data-select-row="${h(identity)}" tabindex="0" role="button" aria-label="${
        t('selectRow', { number: index + 1 })
      }" aria-pressed="${state.selected.has(identity)}" title="${deletedRows.has(row) ? h(t('deletedDraft')) : ''}">${
        index + 1
      }${deletedRows.has(row) ? ' · ×' : ''}</td>${
        columns.map(c =>
          `<td data-cell="${h(c)}" tabindex="0" class="${
            ['id', 'total', 'customer_id', 'revenue', 'orders'].includes(c)
              ? 'number'
              : ''
          } ${cellOriginals.get(row)?.has(c) ? 'modified' : ''} ${
            state.cellEdit?.row === row && state.cellEdit.column === c
              ? 'editing'
              : ''
          }">${gridCell(row, c)}</td>`
        ).join('')
      }</tr>`;
    }).join('')
  }</tbody></table>${rows.length ? '' : `<div class="empty">${t('noRecords')}</div>`}</div>`;
}

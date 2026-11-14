'use strict';
function redisValue(key) {
  if (state.redisFormat === 'hex') {
    return Array.from(
      key.bytes ?? new TextEncoder().encode(key.value),
      b => b.toString(16).padStart(2, '0'),
    ).join(' ');
  }
  if (state.redisFormat === 'json') {
    try {
      return JSON.stringify(JSON.parse(key.value), null, 2);
    } catch {
      return t('invalidJSONOriginalContentIsUnchanged') + '\n\n'
        + key.value;
    }
  }
  return key.value;
}
function redisClipboardValue(key) {
  const base64 = bytes => btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join(''));
  const encode = text => base64(new TextEncoder().encode(text));
  if (key.type === 'STR') {
    if (!key.bytes) return key.value;
    try {
      return new TextDecoder('utf-8', { fatal: true }).decode(key.bytes);
    } catch {
      return 'base64:' + base64(key.bytes);
    }
  }
  const entries = key.rows.map(row => {
    if (key.type === 'HASH') {
      return { field_b64: encode(row[0]), value_b64: encode(row[1]) };
    }
    if (key.type === 'LIST') {
      return { index: row[0], value_b64: encode(row[1]) };
    }
    if (key.type === 'SET') return { member_b64: encode(row[0]) };
    return { member_b64: encode(row[0]), score: row[1] };
  });
  return JSON.stringify({ type: key.type.toLowerCase(), entries }, null, 2);
}
function redisSelectedClipboard(key, selected) {
  const bytes = value => ({
    type: 'bytes',
    value: btoa(
      Array.from(
        new TextEncoder().encode(value),
        byte => String.fromCharCode(byte),
      ).join(''),
    ),
  });
  const entries = key.rows.filter((row, index) => selected.has(index)).map(
    row => {
      if (key.type === 'HASH') {
        return { field: bytes(row[0]), value: bytes(row[1]) };
      }
      if (key.type === 'LIST') {
        return {
          index: { type: 'integer', value: row[0] },
          value: bytes(row[1]),
        };
      }
      if (key.type === 'SET') return { member: bytes(row[0]) };
      return {
        member: bytes(row[0]),
        score: { type: 'float', value: row[1] },
      };
    },
  );
  return JSON.stringify({ type: key.type.toLowerCase(), entries }, null, 2);
}
function redisKeyClipboard(keys) {
  return JSON.stringify(
    keys.map(key => {
      if (!key.keyBytes) return key.name;
      try {
        return new TextDecoder('utf-8', { fatal: true }).decode(
          key.keyBytes,
        );
      } catch {
        return {
          base64: btoa(
            Array.from(key.keyBytes, byte => String.fromCharCode(byte))
              .join(''),
          ),
        };
      }
    }),
    null,
    2,
  );
}
function redisRows(key) {
  const headers = key.type === 'HASH'
    ? [t('field'), t('value')]
    : key.type === 'LIST'
    ? [t('index'), t('value')]
    : key.type === 'ZSET'
    ? [t('member'), t('score')]
    : [t('member')];
  return `<table id="redis-grid" role="grid"><thead><tr><th class="row-number">#</th>${
    headers.map((label, i) =>
      state.hiddenColumns['redis:' + key.type]?.includes(String(i))
        ? ''
        : `<th data-grid-column="${i}">${h(label)}</th>`
    ).join('')
  }</tr></thead><tbody>${
    key.rows.map((row, index) =>
      `<tr class="${
        state.selectedEntries.has(index) ? 'selected' : ''
      }" data-redis-row="${index}"><td class="row-number" data-redis-row-number="${index}" tabindex="-1" role="button" aria-label="${
        h(t('selectRow', { number: index + 1 }))
      }" aria-pressed="${state.selectedEntries.has(index)}">${index + 1}</td>${
        row.map((value, i) =>
          state.hiddenColumns['redis:' + key.type]?.includes(String(i))
            ? ''
            : `<td data-cell="${i}" tabindex="${
              index === 0
                && i
                  === headers.findIndex((_, column) =>
                    !state.hiddenColumns['redis:' + key.type]?.includes(String(column))
                  )
                ? '0'
                : '-1'
            }">${h(value)}</td>`
        ).join('')
      }</tr>`
    ).join('')
  }</tbody></table><div class="table-footer"><span class="spacer"></span>${
    ib(
      'copy',
      t('copyCells'),
      'copy-cells',
      state.cellSelection ? '' : 'disabled',
    )
  }</div>`;
}

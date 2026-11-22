'use strict';
// 只使用设计稿内的示例目录；不请求数据库或平台接口。
(() => {
  let current = null;
  let composing = false;
  const field = target => target?.matches?.('#sql-query, [data-af="sql"]');
  const unquote = value => value.replace(/^`|`$/g, '').replace(/``/g, '`');
  const quote = value => `\`${value.replace(/`/g, '``')}\``;
  const functions = [
    ['COUNT', 'COUNT(expression)'],
    ['SUM', 'SUM(expression)'],
    ['AVG', 'AVG(expression)'],
    ['MIN', 'MIN(expression)'],
    ['MAX', 'MAX(expression)'],
    ['CONCAT', 'CONCAT(string, ...)'],
    ['COALESCE', 'COALESCE(value, ...)'],
    ['IFNULL', 'IFNULL(value, fallback)'],
    ['NOW', 'NOW()'],
    ['ROUND', 'ROUND(number, decimals)'],
  ];
  function close() {
    document.getElementById('sql-completion-popup')?.remove();
    current?.input.removeAttribute('aria-activedescendant');
    current = null;
  }
  function draw() {
    const popup = document.getElementById('sql-completion-popup');
    if (!popup || !current) return;
    popup.innerHTML =
      current.items.map((item, index) =>
        `<button role="option" id="sql-candidate-${index}" aria-selected="${
          index === current.selected
        }" data-sql-candidate="${index}">${h(item.name)}<span>${h(item.kind)}</span></button>`
      ).join('')
      + `<div class="sql-completion-detail">${h(current.items[current.selected].detail)}<br>${
        az('示例元数据 · 未发送请求', 'Sample metadata · no requests sent')
      }</div>`;
    current.input.setAttribute('aria-activedescendant', `sql-candidate-${current.selected}`);
    popup.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }
  function open(input, manual = false) {
    close();
    if (!field(input) || composing || input.selectionStart !== input.selectionEnd) return;
    const cursor = input.selectionStart, text = input.value;
    const parsed = scanSql(text);
    if (parsed.error && !parsed.error.includes(t('unclosedQuote'))) return;
    if (
      parsed.tokens.some(token =>
        ['string', 'comment'].includes(token.kind) && cursor > token.start && cursor <= token.end
      )
    ) return;
    const separators = parsed.tokens.filter(token => token.text === ';');
    const scopeStart = separators.filter(token => token.end <= cursor).at(-1)?.end ?? 0;
    const scopeEnd = separators.find(token => token.start >= cursor)?.start ?? text.length;
    const token = parsed.tokens.find(token =>
      ['word', 'keyword', 'identifier'].includes(token.kind) && cursor >= token.start && cursor <= token.end
    );
    const start = token?.start ?? cursor, end = token?.end ?? cursor;
    const prefix = unquote(text.slice(start, cursor));
    const before = text.slice(scopeStart, start);
    const qualifier = before.match(/(`(?:``|[^`])+`|[\p{L}\p{N}_$]+)\.$/u)?.[1];
    const relationPosition = /\b(?:FROM|JOIN|UPDATE|INTO)\s*$/i.test(before);
    if (!manual && !prefix && !qualifier && !relationPosition) return;
    const database = qualifier && relationPosition ? unquote(qualifier) : state.database;
    const catalog = databases[database] || {};
    let depth = 0;
    const tokens = parsed.tokens.filter(token => {
      if (token.start < scopeStart || token.end > scopeEnd || token.kind === 'comment') return false;
      if (token.text === '(') {
        depth += 1;
        return false;
      }
      if (token.text === ')') {
        depth -= 1;
        return false;
      }
      return depth === 0 && (token.kind || token.text.trim());
    });
    const relations = [];
    if (!tokens.some(token => token.text.toUpperCase() === 'WITH')) {
      for (let i = 0; i < tokens.length - 1; i++) {
        if (!['FROM', 'JOIN', 'UPDATE', 'INTO'].includes(tokens[i].text.toUpperCase())) continue;
        const name = unquote(tokens[i + 1].text);
        if (!(name in catalog)) continue;
        let alias = tokens[i + 2];
        if (alias?.text.toUpperCase() === 'AS') alias = tokens[i + 3];
        relations.push({
          name,
          alias: alias && ['word', 'identifier'].includes(alias.kind) ? unquote(alias.text) : name,
        });
      }
    }
    const items = [];
    if (relationPosition) {
      for (const name of Object.keys(catalog)) {
        items.push({ name, insert: quote(name), kind: az('表', 'Table'), detail: `${database}.${name}` });
      }
      if (!qualifier) {
        for (const name of Object.keys(databases)) {
          items.push({ name, insert: quote(name), kind: az('库', 'Database'), detail: name });
        }
      }
    } else if (qualifier || relations.length) {
      const targets = qualifier ? relations.filter(relation => relation.alias === unquote(qualifier)) : relations;
      if (previewSource !== 'archery' || qualifier || targets.length === 1) {
        for (const target of targets) {
          for (const name of Object.keys(catalog[target.name][0] || {})) {
            items.push({
              name,
              insert: !qualifier && targets.length > 1 ? `${quote(target.alias)}.${quote(name)}` : quote(name),
              kind: az('字段', 'Column'),
              detail: `${database}.${target.name}.${name}`,
            });
          }
        }
      }
    }
    if (!relationPosition && !qualifier) {
      for (const name of sqlKeywords) items.push({ name, insert: name, kind: az('关键词', 'Keyword'), detail: name });
      for (const [name, signature] of functions) {
        items.push({ name, insert: name, kind: az('函数', 'Function'), detail: signature });
      }
    }
    const matched = items.filter(item => item.name.toLowerCase().startsWith(prefix.toLowerCase()))
      .sort((a, b) => a.name.localeCompare(b.name) || a.detail.localeCompare(b.detail)).slice(0, 50);
    if (!matched.length) return;
    current = { input, text, cursor, start, end, items: matched, selected: 0 };
    const popup = document.createElement('div');
    popup.id = 'sql-completion-popup';
    popup.className = 'sql-completion-popup';
    popup.setAttribute('role', 'listbox');
    popup.setAttribute('aria-label', az('SQL 补全', 'SQL completion'));
    const rect = input.getBoundingClientRect(), style = getComputedStyle(input);
    const canvas = document.createElement('canvas').getContext('2d');
    canvas.font = style.font;
    const lines = text.slice(0, cursor).split('\n');
    popup.style.left = `${
      Math.max(
        8,
        Math.min(
          rect.left + canvas.measureText(lines.at(-1)).width - input.scrollLeft + parseFloat(style.paddingLeft),
          innerWidth - 350,
        ),
      )
    }px`;
    const lineHeight = parseFloat(style.lineHeight);
    popup.style.top = `${
      Math.max(
        8,
        Math.min(
          rect.top + lines.length * lineHeight - input.scrollTop + parseFloat(style.paddingTop),
          innerHeight - 280,
        ),
      )
    }px`;
    document.body.append(popup);
    draw();
  }
  function accept(index) {
    const frozen = current;
    if (
      !frozen || !frozen.input.isConnected || frozen.input.value !== frozen.text
      || frozen.input.selectionStart !== frozen.cursor || frozen.input.selectionEnd !== frozen.cursor
    ) return close();
    const item = frozen.items[index];
    close();
    frozen.input.focus();
    frozen.input.setSelectionRange(frozen.start, frozen.end);
    document.execCommand('insertText', false, item.insert);
    close();
  }
  document.addEventListener('input', event => {
    if (field(event.target) && !event.isComposing) open(event.target);
  });
  document.addEventListener('compositionstart', event => {
    if (field(event.target)) {
      composing = true;
      close();
    }
  });
  document.addEventListener('compositionend', event => {
    if (field(event.target)) {
      composing = false;
      open(event.target);
    }
  });
  document.addEventListener('keydown', event => {
    if (!field(event.target) || composing || event.isComposing) return;
    if (event.ctrlKey && !event.shiftKey && !event.metaKey && !event.altKey && event.code === 'Space') {
      event.preventDefault();
      event.stopImmediatePropagation();
      open(event.target, true);
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || !current) return;
    if (['ArrowDown', 'ArrowUp', 'Tab', 'Enter', 'Escape'].includes(event.key)) {
      event.preventDefault();
      event.stopImmediatePropagation();
      if (event.key === 'Escape') close();
      else if (event.key === 'Tab' || event.key === 'Enter') accept(current.selected);
      else {
        current.selected = (current.selected + (event.key === 'ArrowDown' ? 1 : -1) + current.items.length)
          % current.items.length;
        draw();
      }
    } else if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) close();
  }, true);
  document.addEventListener('mousedown', event => {
    const option = event.target.closest('[data-sql-candidate]');
    if (option) {
      event.preventDefault();
      event.stopImmediatePropagation();
      accept(Number(option.dataset.sqlCandidate));
    } else if (event.target.closest('[data-action="sql-completion"]')) {
      event.preventDefault();
      event.stopImmediatePropagation();
      const input = document.querySelector('#sql-query, [data-af="sql"]');
      input?.focus();
      if (input) open(input, true);
    } else if (!field(event.target)) close();
  }, true);
  document.addEventListener('scroll', event => {
    if (field(event.target)) close();
  }, true);
})();

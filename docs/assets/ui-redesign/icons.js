'use strict';
const paths = {
  database: '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 4 16 4 16 0V5M4 12c0 4 16 4 16 0"/>',
  layers: '<path d="m12 3 9 5-9 5-9-5 9-5Zm-9 9 9 5 9-5M3 16l9 5 9-5"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  repeat: '<path d="M20 10a8 8 0 0 0-14-4L3 9m0-6v6h6M4 14a8 8 0 0 0 14 4l3-3m0 6v-6h-6"/>',
  gear:
    '<path d="m9 3 1-1h4l1 4 3-1 3 4-2 3 2 3-3 4-3-1-1 4h-4l-1-4-3 1-3-4 2-3-2-3 3-4 3 1Z"/><circle cx="12" cy="12" r="3"/>',
  x: '<path d="m6 6 12 12M18 6 6 18"/>',
  'arrow-left': '<path d="M19 12H5m6-6-6 6 6 6"/>',
  'arrow-right': '<path d="M5 12h14m-6-6 6 6-6 6"/>',
  chart: '<path d="M4 20V4m0 16h16M8 16v-5m4 5V7m4 9v-3"/>',
  wand: '<path d="m4 20 12-12 4 4L8 24M14 10l4 4M6 3v4M4 5h4M18 2v4m-2-2h4"/>',
  scissors: '<circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="m8 8 12 12M8 16 20 4"/>',
  selection: '<rect x="4" y="4" width="16" height="16" rx="2" stroke-dasharray="3 2"/>',
  folder: '<path d="M3 6h7l2 2h9v11H3z"/>',
  upload: '<path d="M12 16V4m-5 5 5-5 5 5M4 16v4h16v-4"/>',
  'check-square': '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="m7 12 3 3 7-7"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  'file-search':
    '<path d="M13 3H5v18h7M13 3v5h5l-5-5Z"/><circle cx="16" cy="15" r="4"/><path d="m19 18 3 3M8 11h3M8 15h2"/>',
  'clear-square': '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="m8 8 8 8m0-8-8 8"/>',
  'chev-r': '<path d="m9 5 7 7-7 7"/>',
  chev: '<path d="m5 9 7 7 7-7"/>',
  'chev-up': '<path d="m5 15 7-7 7 7"/>',
  back: '<path d="m15 5-7 7 7 7"/>',
  'frame-prev': '<path d="M5 5v14m14-14-9 7 9 7"/>',
  'frame-next': '<path d="M19 5v14M5 5l9 7-9 7"/>',
  power: '<path d="M12 2v10M6 5a9 9 0 1 0 12 0"/>',
  keyboard:
    '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 9h.01M11 9h.01M15 9h.01M19 9h.01M7 12h.01M11 12h.01M15 12h.01M19 12h.01M8 16h8"/>',
  table: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 10h18M3 15h18M9 10v10M15 10v10"/>',
  list: '<path d="M9 6h12M9 12h12M9 18h12M4 6h1M4 12h1M4 18h1"/>',
  tree:
    '<path d="M5 4v12a3 3 0 0 0 3 3h3M5 9h6"/><rect x="11" y="6" width="9" height="6" rx="1"/><rect x="11" y="16" width="9" height="6" rx="1"/>',
  terminal: '<path d="m5 6 5 6-5 6m8 0h6"/>',
  history: '<path d="M3 10a9 9 0 1 1 1 8M3 4v6h6m3-4v6l4 2"/>',
  copy: '<rect x="8" y="8" width="13" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>',
  edit: '<path d="m16 3 5 5L8 21H3v-5L16 3ZM13 6l5 5"/>',
  trash: '<path d="M4 7h16M10 3h4m-8 4 1 14h10l1-14M10 11v6m4-6v6"/>',
  key: '<circle cx="8" cy="15" r="4"/><path d="m11 12 9-9m-4 4 2 2m-5 1 2 2"/>',
  link: '<path d="M10 13a4 4 0 0 0 6 0l3-3a4 4 0 0 0-6-6l-2 2M14 11a4 4 0 0 0-6 0l-3 3a4 4 0 0 0 6 6l2-2"/>',
  play: '<path d="m7 4 13 8-13 8V4Z"/>',
  stop: '<rect x="5" y="5" width="14" height="14" rx="1"/>',
  download: '<path d="M12 3v12m-5-5 5 5 5-5M4 15v6h16v-6"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v6m0-11v1"/>',
  filter: '<path d="M3 4h18l-7 8v7l-4 2v-9L3 4Z"/>',
  columns: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16M15 4v16"/>',
  eye: '<path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>',
};
const icon = name =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${
    paths[name] || paths.info
  }</svg>`;
const ib = (name, label, action, extra = '') =>
  `<button class="icon-button${name === 'trash' ? ' danger' : ''}${action === 'cli-run' ? ' cli-run' : ''}" title="${
    h(label)
  }" aria-label="${h(label)}" data-action="${h(action)}" ${extra}>${icon(name)}</button>`;
const marker = provider =>
  `<span class="provider-icon ${provider}" title="${provider === 'mysql' ? 'MySQL' : 'Redis'}" aria-hidden="true">${
    provider === 'mysql'
      ? '<img src="assets/providers/mysql.svg" alt="" aria-hidden="true" draggable="false">'
      : '<img src="assets/providers/redis.svg" alt="" aria-hidden="true" draggable="false">'
  }</span>`;

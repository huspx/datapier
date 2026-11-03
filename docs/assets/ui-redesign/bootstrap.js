'use strict';
function captureLanguageInputs() {
  state.languageForm = {
    owner: state.overlay,
    fields: [
      ...document.querySelectorAll(
        '.modal input[id], .modal textarea[id], .modal select[id]',
      ),
    ]
      .map(field => ({
        id: field.id,
        value: field.value,
        checked: field.checked,
      })),
  };
  const input = $('#sql-query'), tab = queryTab();
  if (input && tab) {
    tab.sql = input.value;
    tab.sqlStart = input.selectionStart;
    tab.sqlEnd = input.selectionEnd;
    tab.sqlDirection = input.selectionDirection;
  }
  if (state.connectionForm) {
    document.querySelectorAll('[data-connection-field]').forEach(field => {
      state.connectionForm[field.dataset.connectionField] = field.value;
    });
    const search = $('#connection-db-search');
    if (search) state.connectionForm.dbSearch = search.value;
    document.querySelectorAll('[data-database-choice]').forEach(choice => {
      if (choice.checked) {
        state.connectionForm.selectedDatabases.add(
          choice.dataset.databaseChoice,
        );
      } else {state.connectionForm.selectedDatabases.delete(
          choice.dataset.databaseChoice,
        );}
    });
  }
  for (
    const [id, property] of [
      ['object-filter', 'objectFilter'],
      ['row-filter', 'rowFilter'],
      ['key-filter', 'keyFilter'],
      ['cli-command', 'cliText'],
    ]
  ) {
    const field = document.getElementById(id);
    if (field) state[property] = field.value;
  }
}
window.matchMedia('(max-width: 760px)').addEventListener(
  'change',
  event => {
    state.narrow = event.matches;
    state.compact = event.matches ? true : state.wideCompact;
    render();
  },
);
window.addEventListener('languagechange', () => {
  if (languagePreference !== 'system') return;
  captureLanguageInputs();
  language = systemLanguage();
  translatePreview();
  render();
});
document.documentElement.dataset.theme = $('#preview-theme').value;
window.addEventListener('resize', () => requestAnimationFrame(fitGrid));
translatePreview();
render();

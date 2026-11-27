use super::*;

pub(super) fn editors(app: &App, group: &[Uuid]) -> Node<Msg> {
    let mut split = html::div().class("project-layout-splits");
    for id in group {
        let Some((index, tab)) = app.tabs.iter().enumerate().find(|(_, tab)| tab.id == *id) else {
            continue;
        };
        let name = app.sql_editor_name(index);
        let focus_name = name.clone();
        let undo_name = name.clone();
        let edit_name = name;
        let editor = ui::editor()
            .aria(format!("{} · {}", crate::i18n::tr("SQL 查询"), tab.title))
            .prop("class", "query-code")
            .key(app.sql_editor_key(index, true))
            .text(tab.query.text.clone())
            .cursor(tab.query.cursor)
            .anchor(tab.query.cursor)
            .lang("sql")
            .prop("nowrap", true)
            .on_focus(Msg::Focus(focus_name))
            .on_undo(Msg::Undo(undo_name))
            .on_edit(move |e| {
                Msg::Edit(
                    edit_name.clone(),
                    e.from,
                    e.to,
                    e.text.clone(),
                    e.cursor,
                    e.len,
                )
            });
        let pane = html::div()
            .class(if index == app.active {
                "project-layout-pane active"
            } else {
                "project-layout-pane"
            })
            .key(format!("project-pane-{}", tab.id))
            .child(
                html::button(tab.title.clone())
                    .class("project-layout-title")
                    .on_click(Msg::Tab(index)),
            )
            .child(editor);
        split = split.child(pane);
    }
    split.into()
}

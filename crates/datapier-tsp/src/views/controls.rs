use super::*;
// Tern 0.9.9 的语义 button 不转发 class；工作台轻入口通过公开 el 接收局部样式。
pub(super) fn flat_action(icon: &str, label: &str, action: &str) -> html::El<Msg> {
    let label = crate::i18n::tr(label).into_owned();
    html::button("")
        .key(action.to_owned())
        .prop("title", label.clone())
        .attr("aria-label", label.clone())
        .child(ui::icon(icon.to_owned()))
        .child(html::span(label).class("action-label"))
        .on_click(Msg::Action(action.to_owned()))
}

pub(super) fn button(label: &str, action: &str) -> Node<Msg> {
    let variant = match action {
        "run" | "save-profile" | "submit" | "redis-submit" | "redis-stage" | "import"
        | "set-cell" => ui::ButtonVariant::Primary,
        "discard"
        | "redis-op:delete_key"
        | "redis-op:hash_delete"
        | "redis-op:list_delete"
        | "redis-op:set_remove"
        | "redis-op:zset_remove" => ui::ButtonVariant::Danger,
        "redis-row-delete-confirm" | "redis-key-delete-confirm" => ui::ButtonVariant::Danger,
        _ if action.starts_with(concat!("profile-delete-", "confirm:"))
            || action.starts_with("resource-drop-confirm:") =>
        {
            ui::ButtonVariant::Danger
        }
        "navigation-toggle" | "cli-toggle" | "overlay-close" => ui::ButtonVariant::Quiet,
        _ => ui::ButtonVariant::Default,
    };
    let deletion = action != "discard" && matches!(variant, ui::ButtonVariant::Danger);
    let mut button = ui::button(crate::i18n::tr(label).into_owned())
        .variant(variant)
        .key(action.to_owned())
        .on_click(Msg::Action(action.into()));
    if deletion {
        button = button.icon("trash");
    }
    button.into()
}
pub(super) fn icon_button(icon: &str, label: &str, action: &str) -> Node<Msg> {
    let label = crate::i18n::tr(label).into_owned();
    ui::button("")
        .variant(
            if matches!(
                action,
                "redis-op:delete_key"
                    | "redis-op:hash_delete"
                    | "redis-op:list_delete"
                    | "redis-op:set_remove"
                    | "redis-op:zset_remove"
            ) {
                ui::ButtonVariant::Danger
            } else {
                ui::ButtonVariant::Quiet
            },
        )
        .size(ui::ButtonSize::Md)
        .icon(icon.to_owned())
        .key(action.to_owned())
        .title(label.clone())
        .aria(label)
        .on_click(Msg::Action(action.to_owned()))
        .into()
}
pub(super) fn page_button(icon: &str, label: &str, action: &str, enabled: bool) -> Node<Msg> {
    let label = crate::i18n::tr(label).into_owned();
    ui::button("")
        .variant(ui::ButtonVariant::Quiet)
        .size(ui::ButtonSize::Sm)
        .icon(icon.to_owned())
        .disabled(!enabled)
        .title(label.clone())
        .aria(label)
        .on_click(Msg::Action(action.into()))
        .into()
}
pub(super) fn field(app: &App, name: &str, label: &str) -> Node<Msg> {
    let ed = if name == "cli_text" {
        &app.tabs[app.active].cli_query
    } else if name == "cell" {
        &app.workbench.cell_editor
    } else {
        &app.fields[name]
    };
    let display = ed.text.clone();
    let f = name.to_string();
    let u = f.clone();
    let n = f.clone();
    let required = matches!(name, "name" | "host" | "port" | "path" | "column")
        || (name == "width" && app.workbench.overlay.as_deref() == Some("columns"))
        || (name == "redis_score" && app.field("operation") == "zset_set")
        || (name == "redis_ttl"
            && app.workbench.ttl_policy == crate::workbench::RedisTtlPolicy::Set)
        || (name == "cert" && !app.field("key").is_empty())
        || (name == "key" && !app.field("cert").is_empty());
    let label = crate::i18n::tr(label).into_owned();
    let control: Node<Msg> = if name == "cell"
        || name == "redis_value"
        || name == "cli_text"
        || (name == "redis_member" && matches!(app.field("operation"), "set_add" | "set_remove"))
    {
        ui::field(label.clone())
            .required(required)
            .key(name.to_string())
            .role("datapier.form-field")
            .child(
                ui::editor()
                    .readonly(app.connection_form_busy())
                    .key(format!("input-{name}"))
                    .text(display)
                    .cursor(ed.cursor)
                    .anchor(ed.anchor)
                    .max_lines(16u32)
                    .on_focus(Msg::Focus(f))
                    .on_undo(Msg::Undo(u))
                    .on_edit(move |e| {
                        Msg::Edit(n.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                    }),
            )
            .into()
    } else {
        ui::field(label)
            .required(required)
            .key(name.to_string())
            .role("datapier.form-field")
            .child(
                ui::input()
                    .variant(ui::InputVariant::Field)
                    .readonly(app.connection_form_busy())
                    .secret(name == "password")
                    .key(format!("input-{name}"))
                    .text(display)
                    .cursor(ed.cursor)
                    .anchor(ed.anchor)
                    .on_focus(Msg::Focus(f))
                    .on_undo(Msg::Undo(u))
                    .on_edit(move |e| {
                        Msg::Edit(n.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                    }),
            )
            .into()
    };
    if let Some(error) = app
        .workbench
        .field_error
        .as_ref()
        .filter(|error| error.field == name)
    {
        html::div()
            .child(control)
            .child(html::span(error.message.render().into_owned()).class("field-error"))
            .into()
    } else {
        control
    }
}
pub(super) fn choices(
    app: &App,
    field_name: &str,
    label: &str,
    options: &[(&str, &str)],
) -> Node<Msg> {
    let field_name = field_name.to_owned();
    let error_field = field_name.clone();
    let select_key = format!("field-{field_name}");
    let open = app.workbench.generic_select.as_deref() == Some(select_key.as_str());
    let mut select = ui::select()
        .key(format!("input-{field_name}"))
        .value(app.field(&field_name).to_owned())
        .disabled(app.connection_form_busy())
        .open(open)
        .on_toggle(move |collapsed| {
            Msg::Action(format!("select-open:{select_key}:{}", !collapsed))
        });
    for &(value, title) in options {
        select = select.option(ui::SelectOption::new(
            value,
            crate::i18n::tr(title).into_owned(),
        ));
    }
    let label = if field_name == "format" {
        format!("{} *", crate::i18n::tr(label))
    } else {
        crate::i18n::tr(label).into_owned()
    };
    let control: Node<Msg> = ui::field(label)
        .child(select.on_change(move |change| {
            Msg::Action(format!(
                "field:{}:{}",
                field_name,
                change.value.as_str().unwrap_or_default()
            ))
        }))
        .into();
    if let Some(error) = app
        .workbench
        .field_error
        .as_ref()
        .filter(|error| error.field == error_field)
    {
        html::div()
            .child(control)
            .child(html::span(error.message.render().into_owned()).class("field-error"))
            .into()
    } else {
        control
    }
}

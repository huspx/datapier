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
        | "set-cell" | "file-save" | "column-width" | "paste-confirm" => ui::ButtonVariant::Primary,
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
        "navigation-toggle" | "cli-toggle" => ui::ButtonVariant::Quiet,
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
            ) || action.starts_with("server-delete:")
            {
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
        .size(ui::ButtonSize::Md)
        .icon(icon.to_owned())
        .disabled(!enabled)
        .title(label.clone())
        .aria(label)
        .on_click(Msg::Action(action.into()))
        .into()
}

pub(super) fn field(app: &App, name: &str, label: &str) -> Node<Msg> {
    field_control(app, name, label, None)
}

pub(super) fn field_with_hint(app: &App, name: &str, label: &str, hint: &str) -> Node<Msg> {
    field_control(app, name, label, Some(hint))
}

fn field_control(app: &App, name: &str, label: &str, hint: Option<&str>) -> Node<Msg> {
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
    let required = matches!(
        name,
        "name" | "host" | "port" | "path" | "column" | "mcp-rule-scope"
    ) || (name.starts_with("settings-") && !name.starts_with("settings-history-"))
        || matches!(
            name,
            "editing_plan_items"
                | "editing_plan_bytes"
                | "editing_value_bytes"
                | "editing_text_bytes"
                | "editing_preview_rows"
        )
        || (matches!(name, "external_program" | "external_args")
            && app.field("external_mode") != "disabled")
        || matches!(name, "archery_name" | "archery_url" | "archery_user")
        || name.starts_with("archery_profile_name_")
        || (name == "password"
            && app.workbench.overlay.as_deref() == Some("archery-auth")
            && (!app.archery.saved
                || matches!(app.archery.state.as_str(), "auth_required" | "auth_failed")
                || (app.archery.server_only && app.archery_password_policy_changed())))
        || name == "archery_otp"
        || (name == "archery_totp_uri"
            && ((app.archery.totp_automatic
                && (app.archery.clear_totp
                    || app
                        .archery
                        .server
                        .as_ref()
                        .is_none_or(|server| server["totp_configured"] != true)))
                || (app.archery.server_only && app.archery_totp_policy_changed())))
        || (name == "width" && app.workbench.overlay.as_deref() == Some("columns"))
        || (name == "redis_score" && app.field("operation") == "zset_set")
        || (name == "redis_ttl"
            && app.workbench.ttl_policy == crate::workbench::RedisTtlPolicy::Set)
        || (name == "cert" && !app.field("key").is_empty())
        || (name == "key" && !app.field("cert").is_empty());
    let accessible_label = crate::i18n::tr(if name == "cli_text" && label.is_empty() {
        "Redis CLI"
    } else {
        label
    })
    .into_owned();
    let accessible_label = if required {
        crate::i18n::format_message("{label}（必填）", &[("label", &accessible_label)])
    } else {
        accessible_label
    };
    let mut control = ui::field(crate::i18n::tr(label).into_owned())
        .required(required)
        .key(name.to_string())
        .role("datapier.form-field");
    if let Some(hint) = hint {
        control = control.hint(crate::i18n::tr(hint).into_owned());
    }
    if app.workbench.overlay.as_deref() == Some("settings")
        && !matches!(name, "external_program" | "external_args")
    {
        control = control.variant(ui::FieldVariant::Row);
    }
    if let Some(error) = app
        .workbench
        .field_error
        .as_ref()
        .filter(|error| error.field == name)
    {
        control = control.error(error.message.render().into_owned());
    }
    if matches!(
        name,
        "cell"
            | "redis_value"
            | "redis_member"
            | "cli_text"
            | "external_args"
            | "mapping"
            | "binary_columns"
    ) {
        control
            .child(
                ui::editor()
                    .aria(accessible_label)
                    .readonly(
                        app.connection_form_busy()
                            || (name == "cli_text"
                                && (app.workbench.cli_running || app.operation_pending)),
                    )
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
        control
            .child(
                ui::input()
                    .aria(accessible_label)
                    .variant(ui::InputVariant::Field)
                    .readonly(
                        app.connection_form_busy()
                            || (name == "mcp-rule-scope"
                                && !app.field("mcp-rule-pending").is_empty()),
                    )
                    .secret(matches!(
                        name,
                        "password" | "archery_otp" | "archery_totp_uri"
                    ))
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
    }
}
pub(super) fn choices(
    app: &App,
    field_name: &str,
    label: &str,
    options: &[(&str, &str)],
) -> Node<Msg> {
    let label = crate::i18n::tr(label).into_owned();
    let field_name = field_name.to_owned();
    let error_field = field_name.clone();
    let select_key = format!("field-{field_name}");
    let open = app.workbench.generic_select.as_deref() == Some(select_key.as_str());
    let mut select = ui::select()
        .key(format!("input-{field_name}"))
        .aria(label.clone())
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
    let mut field = ui::field(label).required(field_name == "format");
    if app.workbench.overlay.as_deref() == Some("settings") {
        field = field.variant(ui::FieldVariant::Row);
    }
    let control: Node<Msg> = field
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

use super::*;

pub(super) fn profile_form(app: &App) -> (html::El<Msg>, html::El<Msg>) {
    let mut panel = html::div().class("modal-body");
    let footer;
    let mut form = html::div()
        .class("profile-form")
        .key(format!("profile-form-{:?}", app.form_id))
        .child({
            let mut cards = html::div()
                .class("provider-cards")
                .attr("role", "radiogroup")
                .attr("aria-label", crate::i18n::tr("数据库类型").into_owned());
            for (value, name) in [("mysql", "MySQL"), ("redis", "Redis")] {
                let selected = app.field("provider") == value;
                cards = cards.child(
                    html::button("")
                        .class(if selected {
                            "provider-card selected"
                        } else {
                            "provider-card"
                        })
                        .key(format!("input-provider-{value}"))
                        .attr("role", "radio")
                        .attr("aria-checked", selected)
                        .disabled(app.connection_form_busy())
                        .on_focus(Msg::Focus(format!("provider-{value}")))
                        .on_click(Msg::Action(format!("field:provider:{value}")))
                        .child(provider_icon(app, value == "redis"))
                        .child(html::span(name)),
                );
            }
            html::div()
                .class("provider-choice")
                .child(html::span(crate::i18n::tr("数据库类型").into_owned()).class("label"))
                .child(cards)
        })
        .child(field(app, "name", "连接名称"))
        .child(
            html::div()
                .class("form-grid")
                .child(field(app, "host", "主机"))
                .child(field(app, "port", "端口")),
        )
        .child(
            html::div()
                .class("form-grid")
                .child(field(app, "user", "用户名"))
                .child(
                    html::div()
                        .class("profile-password")
                        .child(field(app, "password", "密码"))
                        .child(
                            html::span(
                                crate::i18n::tr("保存到系统凭据库；不可用时仅保留当前会话。")
                                    .into_owned(),
                            )
                            .class("profile-note"),
                        ),
                ),
        );
    if app.field("provider") == "mysql" {
        form = form.child(field(app, "database", "默认数据库"));
        let state = &app.navigation.form;
        let mut databases = html::div().class("profile-databases").child(
            html::div()
                .class("toolbar")
                .child(html::strong(crate::i18n::tr("显示数据库").into_owned()))
                .child(html::span("").class("spacer"))
                .child(page_button(
                    "repeat",
                    if state.loading {
                        "加载中…"
                    } else {
                        "加载数据库"
                    },
                    "profile-databases-load",
                    !app.connection_form_busy() && !state.loading,
                )),
        );
        if state.loaded {
            databases = databases
                .child(field(app, "database_search", "搜索数据库"))
                .child(
                    html::div()
                        .class("toolbar")
                        .child(
                            html::span(format!(
                                "{} / {}",
                                state.selected.len(),
                                state.candidates.len()
                            ))
                            .class("muted"),
                        )
                        .child(html::span("").class("spacer"))
                        .child(page_button(
                            "check",
                            "选中匹配项",
                            "profile-databases-select-matching",
                            !app.connection_form_busy(),
                        ))
                        .child(page_button(
                            "x",
                            "取消匹配项",
                            "profile-databases-clear-matching",
                            !app.connection_form_busy(),
                        )),
                );
            let needle = app.field("database_search").to_lowercase();
            let mut candidates = html::div().class("database-candidates");
            for (index, name) in state.candidates.iter().enumerate() {
                if name.to_lowercase().contains(&needle) {
                    candidates = candidates.child(
                        html::label()
                            .class("database-choice")
                            .child(
                                html::checkbox("database", name.clone())
                                    .key(format!("database-{index}"))
                                    .checked(state.selected.contains(name))
                                    .disabled(app.connection_form_busy())
                                    .on_change(move |_| {
                                        Msg::Action(format!("profile-databases-toggle:{index}"))
                                    }),
                            )
                            .child(html::span(name.clone())),
                    );
                }
            }
            databases = databases.child(candidates);
        } else {
            databases = databases.child(
                html::span(crate::i18n::tr("默认显示全部可访问数据库").into_owned()).class("muted"),
            );
        }
        if let Some(error) = &state.error {
            databases = databases.child(html::span(error.clone()).class("connection-error"));
        }
        form = form.child(databases);
    } else {
        form = form.child(
            ui::field(crate::i18n::tr("默认 DB").into_owned()).child(redis_db_picker(app, true)),
        );
    }
    let read_only = app.field("access") == "read_only";
    form = form
        .child(
            ui::switch(crate::i18n::tr("以只读模式连接").into_owned())
                .on(read_only)
                .disabled(app.connection_form_busy())
                .on_change(|change| {
                    Msg::Action(if change.value.as_bool().unwrap_or(false) {
                        "field:access:read_only".into()
                    } else {
                        "field:access:read_write".into()
                    })
                }),
        )
        .child(
            html::button(
                crate::i18n::tr(if app.workbench.profile_tls_details {
                    "▾ TLS 与证书"
                } else {
                    "▸ TLS 与证书"
                })
                .into_owned(),
            )
            .class("tls-disclosure")
            .attr("aria-expanded", app.workbench.profile_tls_details)
            .attr("disabled", app.connection_form_busy())
            .on_click(Msg::Action("profile-tls-toggle".into())),
        );
    if app.workbench.profile_tls_details {
        form = form.child(
            html::div().class("readonly-toggle").child(
                ui::switch("TLS")
                    .key("input-tls")
                    .on(app.field("tls") == "true")
                    .disabled(app.connection_form_busy())
                    .on_change(|change| {
                        Msg::Action(format!(
                            "field:tls:{}",
                            change.value.as_bool().unwrap_or(false)
                        ))
                    }),
            ),
        );
        if let Some(error) = app
            .workbench
            .field_error
            .as_ref()
            .filter(|error| error.field == "tls")
        {
            form = form.child(html::span(error.message.render().into_owned()).class("notice"));
        }
        if app.field("tls") == "true" {
            form = form.child(field(app, "ca", "CA 证书路径")).child(
                html::div()
                    .class("form-grid")
                    .child(field(app, "cert", "客户端证书路径"))
                    .child(field(app, "key", "客户端私钥路径")),
            );
        }
    }
    panel = panel.child(form);
    if let Some(error) = &app.workbench.profile_error {
        panel = panel.child(
            html::div()
                .class("connection-error")
                .child(html::span(error.clone())),
        );
    }
    let busy = app.connection_form_busy();
    let editing = app.connection_form_is_editing();
    footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(
            ui::button(crate::i18n::tr("关闭").into_owned())
                .variant(ui::ButtonVariant::Quiet)
                .disabled(busy)
                .on_click(Msg::Action("overlay-close".into())),
        )
        .child(
            ui::button(crate::i18n::tr("测试连接").into_owned())
                .disabled(busy)
                .on_click(Msg::Action("test-profile".into())),
        )
        .child(
            ui::button(
                crate::i18n::tr(if editing && app.connection_form_has_tabs() {
                    "保存并重连"
                } else if editing {
                    "保存"
                } else {
                    "保存连接"
                })
                .into_owned(),
            )
            .variant(ui::ButtonVariant::Primary)
            .busy(busy)
            .on_click(Msg::Action("save-profile".into())),
        );
    (panel, footer)
}

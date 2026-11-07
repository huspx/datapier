//! 普通终端的数据源与设置管理界面。

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use datapier_core::{AppError, Result, model::ClientKind};
use datapier_engine::{Client, ClientOptions};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::mpsc;
use uuid::Uuid;

mod forms;
use forms::Form;

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

/// 连接共享 Engine 并运行键盘驱动的配置界面；退出或发生错误时恢复终端。
pub async fn run(root: PathBuf, workspace: String) -> Result<()> {
    let client = Client::connect_with(
        &root,
        ClientOptions {
            kind: ClientKind::Tui,
            workspace,
            ..Default::default()
        },
    )
    .await?;
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(client.clone(), tx);
    app.request("profiles", "profile.list", json!({}));
    app.request("settings_load", "settings.get", json!({}));
    let mut events = EventStream::new();
    let mut engine_events = client.subscribe_events();
    let mut events_open = true;
    let result = loop {
        if let Err(error) = terminal.draw(|frame| app.render(frame)) {
            break Err(AppError::from(error));
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) { break Ok(()); }
                    if app.key(key.code, key.modifiers) { break Ok(()); }
                }
                Some(Ok(_)) => {},
                Some(Err(error)) => break Err(error.into()),
                None => break Ok(()),
            },
            Some((tag, response)) = rx.recv() => app.response(&tag, response),
            event = engine_events.recv(), if events_open => match event { Ok(event) => match event.kind.as_str() { "connections.changed" => app.request("profiles", "profile.list", json!({})), "settings.changed" if app.screen != Screen::Settings => app.request("settings_load", "settings.get", json!({})), _ => {} }, Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { app.request("profiles", "profile.list", json!({})); if app.screen != Screen::Settings { app.request("settings_load", "settings.get", json!({})); } }, Err(tokio::sync::broadcast::error::RecvError::Closed) => events_open = false },
        }
    };
    drop(_guard);
    let close = client.close().await;
    result.and(close)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Direct,
    Settings,
    Servers,
    Server,
    Instances,
    Databases,
    ProfileName,
    Preview,
    Rules,
    Cache,
    Confirm,
}

struct App {
    client: Client,
    tx: mpsc::UnboundedSender<(String, Result<Value>)>,
    screen: Screen,
    previous: Screen,
    cursor: usize,
    scroll: usize,
    status: String,
    profiles: Vec<Value>,
    servers: Vec<Value>,
    settings: Value,
    form: Form,
    selected_profile: Option<Value>,
    server: Value,
    auth: Value,
    instances: Vec<Value>,
    instance: Value,
    databases: Vec<String>,
    selected_databases: Vec<String>,
    desired: Vec<Value>,
    baseline: Vec<Value>,
    rules: Vec<Value>,
    cache: Value,
    pending: usize,
    english: bool,
    confirm: String,
    staged_credentials: Option<Value>,
    default_database: Option<String>,
    rules_revision: u64,
}
impl App {
    fn new(client: Client, tx: mpsc::UnboundedSender<(String, Result<Value>)>) -> Self {
        Self {
            client,
            tx,
            screen: Screen::Home,
            previous: Screen::Home,
            cursor: 0,
            scroll: 0,
            status: String::new(),
            profiles: vec![],
            servers: vec![],
            settings: Value::Null,
            form: Form::default(),
            selected_profile: None,
            server: Value::Null,
            auth: Value::Null,
            instances: vec![],
            instance: Value::Null,
            databases: vec![],
            selected_databases: vec![],
            desired: vec![],
            baseline: vec![],
            rules: vec![],
            cache: Value::Null,
            pending: 0,
            english: false,
            confirm: String::new(),
            staged_credentials: None,
            default_database: None,
            rules_revision: 0,
        }
    }
    fn text<'a>(&self, zh: &'a str, en: &'a str) -> &'a str {
        if self.english { en } else { zh }
    }
    fn request(&mut self, tag: &str, method: &str, params: Value) {
        self.pending += 1;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let tag = tag.to_owned();
        let method = method.to_owned();
        tokio::spawn(async move {
            let result = client.request(&method, params).await;
            let _ = tx.send((tag, result));
        });
    }
    fn navigate(&mut self, screen: Screen) {
        self.screen = screen;
        self.cursor = 0;
        self.scroll = 0;
    }
    fn response(&mut self, tag: &str, result: Result<Value>) {
        self.pending = self.pending.saturating_sub(1);
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        self.status = self.text("操作完成", "Done").into();
        match tag {
            "profiles" => self.profiles = value["profiles"].as_array().cloned().unwrap_or_default(),
            "settings_load" => {
                self.settings = value;
                self.set_language();
            }
            "settings_edit" => {
                self.settings = value;
                self.set_language();
                self.form = forms::settings(&self.settings);
                self.navigate(Screen::Settings);
            }
            "settings_save" => {
                self.request("settings_load", "settings.get", json!({}));
                self.navigate(Screen::Home);
            }
            "direct_save" | "delete" => {
                self.navigate(Screen::Home);
                self.request("profiles", "profile.list", json!({}));
                self.status = if tag == "direct_save" && value["credential_saved"] == false {
                    self.text(
                        "已保存；凭据仅保存在当前会话或未提供",
                        "Saved; credential is session-only or absent",
                    )
                    .into()
                } else {
                    self.status.clone()
                };
            }
            "direct_test" => {
                self.status = self
                    .text("连接测试成功", "Connection test succeeded")
                    .into()
            }
            "servers" => {
                self.servers = value["servers"].as_array().cloned().unwrap_or_default();
                self.navigate(Screen::Servers);
            }
            "server_saved" => {
                self.server = value;
                self.form = forms::server(Some(&self.server));
                self.status = self.text("服务器已保存", "Server saved").into();
                self.request("profiles", "profile.list", json!({}));
            }
            "server_deleted" => {
                self.request("servers", "archery.server.list", json!({}));
                self.request("profiles", "profile.list", json!({}));
            }
            "auth" => {
                self.auth = value;
                self.status = format!(
                    "{}: {}",
                    self.text("认证状态", "Authentication"),
                    self.auth["state"].as_str().unwrap_or("?")
                );
            }
            "instances" => {
                self.instances = value["instances"].as_array().cloned().unwrap_or_default();
                self.navigate(Screen::Instances);
            }
            "databases" => {
                self.databases = value["databases"]
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                for d in &self.selected_databases {
                    if !self.databases.contains(d) {
                        self.databases.push(d.clone());
                    }
                }
                self.navigate(Screen::Databases);
            }
            "batch" => {
                self.navigate(Screen::Home);
                self.request("profiles", "profile.list", json!({}));
            }
            "rules" => {
                self.rules_revision = value["revision"].as_u64().unwrap_or(0);
                self.rules = value["rules"].as_array().cloned().unwrap_or_default();
                self.form = forms::rules(&self.rules, self.selected_profile.as_ref().unwrap());
                self.navigate(Screen::Rules);
            }
            "rules_save" => {
                self.navigate(Screen::Home);
                self.request("profiles", "profile.list", json!({}));
            }
            "cache" => {
                self.cache = value;
                self.navigate(Screen::Cache);
            }
            "cache_clear" => {
                self.cache = value;
                self.navigate(Screen::Cache);
            }
            _ => {}
        }
    }
    fn set_language(&mut self) {
        self.english = match self.settings["language"].as_str() {
            Some("en") => true,
            Some("zh-CN") => false,
            _ => !std::env::var("LC_ALL")
                .or_else(|_| std::env::var("LANG"))
                .unwrap_or_default()
                .to_lowercase()
                .starts_with("zh"),
        };
    }
    fn selected(&self) -> Option<Value> {
        self.profiles.get(self.cursor).cloned()
    }
    fn confirm(&mut self, action: &str) {
        self.confirm = action.into();
        self.previous = self.screen;
        self.navigate(Screen::Confirm);
    }
    fn render(&mut self, f: &mut ratatui::Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(f.area());
        let title = match self.screen {
            Screen::Home => self.text("连接", "Connections"),
            Screen::Direct => "Direct",
            Screen::Settings => self.text("全局设置", "Global settings"),
            Screen::Servers | Screen::Server => self.text("Archery 服务", "Archery servers"),
            Screen::Instances => self.text("选择实例", "Select instances"),
            Screen::Databases => self.text("选择数据库", "Select databases"),
            Screen::ProfileName => self.text("连接名称", "Connection name"),
            Screen::Preview => self.text("批量变更预览", "Batch preview"),
            Screen::Rules => self.text("MCP 权限规则", "MCP access rules"),
            Screen::Cache => self.text("旧结果缓存", "Retired result cache"),
            Screen::Confirm => self.text("确认", "Confirm"),
        };
        f.render_widget(
            Paragraph::new(format!(
                "Datapier  |  {title}{}",
                if self.pending > 0 { "  …" } else { "" }
            ))
            .block(Block::default().borders(Borders::ALL)),
            areas[0],
        );
        let mut rows = Vec::<String>::new();
        match self.screen {
            Screen::Home => {
                for p in &self.profiles {
                    rows.push(format!(
                        "{}  [{} / {}]  {}",
                        p["name"].as_str().unwrap_or("?"),
                        p["provider"].as_str().unwrap_or("?"),
                        p["connection"]["kind"].as_str().unwrap_or("?"),
                        p["id"].as_str().unwrap_or("")
                    ));
                }
                if rows.is_empty() {
                    rows.push(self.text("暂无连接", "No connections").into());
                }
            }
            Screen::Servers => {
                for s in &self.servers {
                    rows.push(format!(
                        "{}  {}  {}",
                        s["name"].as_str().unwrap_or("?"),
                        s["base_url"].as_str().unwrap_or("?"),
                        s["username"].as_str().unwrap_or("?")
                    ));
                }
                if rows.is_empty() {
                    rows.push(self.text("暂无服务", "No servers").into());
                }
            }
            Screen::Direct
            | Screen::Server
            | Screen::Settings
            | Screen::Rules
            | Screen::ProfileName => {
                for field in &self.form.fields {
                    rows.push(field.display(self.english));
                }
            }
            Screen::Instances => {
                for i in &self.instances {
                    let chosen = self
                        .desired
                        .iter()
                        .any(|p| p["connection"]["instance_id"] == i["id"]);
                    rows.push(format!(
                        "[{}] {} ({})",
                        if chosen { "x" } else { " " },
                        i["name"].as_str().unwrap_or("?"),
                        i["provider"].as_str().unwrap_or("?")
                    ));
                }
            }
            Screen::Databases => {
                for d in &self.databases {
                    rows.push(format!(
                        "[{}{}] {}",
                        if self.selected_databases.contains(d) {
                            "x"
                        } else {
                            " "
                        },
                        if self.default_database.as_ref() == Some(d) {
                            "*"
                        } else {
                            " "
                        },
                        d
                    ));
                }
            }
            Screen::Preview => {
                for p in &self.desired {
                    rows.push(format!(
                        "{}: {}  {}",
                        p["name"].as_str().unwrap_or("?"),
                        p["connection"]["databases"],
                        p["connection"]["default_database"].as_str().unwrap_or("")
                    ));
                }
                for p in &self.baseline {
                    if !self.desired.iter().any(|d| d["id"] == p["id"]) {
                        rows.push(format!("{}: {}", self.text("删除", "Delete"), p["name"]));
                    }
                }
            }
            Screen::Cache => {
                rows.push(format!(
                    "{}: {}",
                    self.text("字节", "Bytes"),
                    self.cache["bytes"]
                ));
                if let Some(paths) = self.cache["paths"].as_array() {
                    for p in paths {
                        rows.push(p.as_str().unwrap_or("?").into());
                    }
                }
            }
            Screen::Confirm => rows.push(
                match self.confirm.as_str() {
                    "profile.delete" => self.text("确认删除此连接？", "Delete this connection?"),
                    "archery.server.delete" => self.text(
                        "确认删除此 Archery 服务及其连接？",
                        "Delete this Archery server and its connections?",
                    ),
                    "archery.profile.apply_batch" => {
                        self.text("确认保存以上连接变更？", "Apply these connection changes?")
                    }
                    "cache.retired.clear" => self.text(
                        "确认清理以上旧结果缓存？",
                        "Clear the listed retired result cache?",
                    ),
                    _ => self.text("确认此操作？", "Confirm this operation?"),
                }
                .into(),
            ),
        }
        let height = areas[1].height.saturating_sub(2) as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if height > 0 && self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        let items: Vec<ListItem> = rows
            .into_iter()
            .enumerate()
            .skip(self.scroll)
            .take(height)
            .map(|(i, s)| {
                let style = if i == self.cursor {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(s)).style(style)
            })
            .collect();
        f.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL)),
            areas[1],
        );
        let help = match self.screen {
            Screen::Home => self.text("↑↓ 选择  n 新建  e 编辑  t 测试  d 删除  a Archery  r 权限  s 设置  c 清理  q 退出", "↑↓ select  n new  e edit  t test  d delete  a Archery  r rules  s settings  c cleanup  q quit"),
            Screen::Direct => self.text("↑↓/Tab 字段  输入编辑  Space 切换  F2 保存  F5 测试  Esc 返回", "↑↓/Tab field  type edit  Space toggle  F2 save  F5 test  Esc back"),
            Screen::Settings => self.text("↑↓/Tab 字段  输入编辑  Space 切换  F2 保存  Esc 返回", "↑↓/Tab field  type edit  Space toggle  F2 save  Esc back"),
            Screen::Rules => self.text("↑↓ 字段  Space 权限  输入库名 F4 添加  F2 保存  Esc 返回", "↑↓ field  Space level  type DB name and F4 add  F2 save  Esc back"),
            Screen::Servers => self.text("↑↓ 选择  Enter 编辑/登录  n 新建  d 删除  Esc 返回", "↑↓ select  Enter edit/login  n new  d delete  Esc back"),
            Screen::Server => self.text("↑↓ 字段  F5 登录  F6 验证OTP  F2 保存服务  F3 实例  Esc 返回", "↑↓ fields  F5 login  F6 verify OTP  F2 save server  F3 instances  Esc back"),
            Screen::Instances => self.text("↑↓ 选择  Enter 选数据库  n 名称  d 移除  p 预览  Esc 返回", "↑↓ select  Enter databases  n name  d remove  p preview  Esc back"),
            Screen::Databases => self.text("↑↓ 选择  Space 多选  d 默认库  Enter 确认  Esc 返回", "↑↓ select  Space toggle  d default  Enter confirm  Esc back"),
            Screen::ProfileName => self.text("输入名称  Enter 确认  Esc 返回", "Type name  Enter confirm  Esc back"),
            Screen::Preview => self.text("F2 确认批量保存  Esc 返回", "F2 apply batch  Esc back"),
            Screen::Cache => self.text("d 清理  Esc 返回", "d clear  Esc back"), Screen::Confirm => self.text("y 确认  n/Esc 取消", "y confirm  n/Esc cancel"),
        };
        f.render_widget(
            Paragraph::new(format!("{help}\n{}", self.status))
                .block(Block::default().borders(Borders::ALL)),
            areas[2],
        );
    }
    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let count = match self.screen {
            Screen::Home => self.profiles.len(),
            Screen::Servers => self.servers.len(),
            Screen::Instances => self.instances.len(),
            Screen::Databases => self.databases.len(),
            Screen::Direct | Screen::Server | Screen::Settings | Screen::Rules => {
                self.form.fields.len()
            }
            _ => 0,
        };
        match code {
            KeyCode::Up | KeyCode::BackTab => {
                self.cursor = self.cursor.saturating_sub(1);
                return false;
            }
            KeyCode::Down | KeyCode::Tab => {
                if count > 0 {
                    self.cursor = (self.cursor + 1).min(count - 1);
                }
                return false;
            }
            _ => {}
        }
        if self.pending > 0
            && (matches!(
                code,
                KeyCode::F(2) | KeyCode::F(3) | KeyCode::F(5) | KeyCode::F(6)
            ) || (self.screen == Screen::Confirm && code == KeyCode::Char('y')))
        {
            return false;
        }
        match self.screen {
            Screen::Home => match code {
                KeyCode::Char('q') => return true,
                KeyCode::Char('n') => {
                    self.form = forms::direct(None, &self.settings["approval"]["default_policy"]);
                    self.navigate(Screen::Direct);
                    self.cursor = 2;
                }
                KeyCode::Char('e') | KeyCode::Enter => {
                    if let Some(p) = self.selected() {
                        if p["connection"]["kind"] == "direct" {
                            self.form = forms::direct(
                                Some(&p),
                                &self.settings["approval"]["default_policy"],
                            );
                            self.navigate(Screen::Direct);
                            self.cursor = 2;
                        } else {
                            self.request("servers", "archery.server.list", json!({}));
                        }
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(p) = self.selected() {
                        self.request("direct_test", "profile.test", json!({"profile":p}));
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(p) = self.selected() {
                        self.selected_profile = Some(p);
                        self.confirm("profile.delete");
                    }
                }
                KeyCode::Char('a') => self.request("servers", "archery.server.list", json!({})),
                KeyCode::Char('s') => self.request("settings_edit", "settings.get", json!({})),
                KeyCode::Char('c') => self.request("cache", "cache.retired.preview", json!({})),
                KeyCode::Char('r') => {
                    if let Some(p) = self.selected() {
                        self.selected_profile = Some(p.clone());
                        self.request("rules", "profile.rules.get", json!({"profile_id":p["id"]}));
                    }
                }
                _ => {}
            },
            Screen::Direct => match code {
                KeyCode::Esc => {
                    self.form = Form::default();
                    self.navigate(Screen::Home);
                }
                KeyCode::F(2) | KeyCode::F(5) => {
                    let profile = match forms::direct_value(&self.form) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = e;
                            return false;
                        }
                    };
                    let password = if code == KeyCode::F(5) {
                        self.form.get("password").to_owned()
                    } else {
                        self.form.take("password")
                    };
                    let action = self.form.get("credential").to_owned();
                    if matches!(action.as_str(), "keyring" | "session") && password.is_empty() {
                        self.status = self.text("请输入密码", "Enter a password").into();
                        return false;
                    }
                    let secret = if password.is_empty() {
                        Value::Null
                    } else {
                        json!(password)
                    };
                    if code == KeyCode::F(5) {
                        self.request(
                            "direct_test",
                            "profile.test",
                            json!({"profile":profile,"secret":secret}),
                        );
                    } else {
                        let update = match action.as_str() {
                            "clear" => json!({"kind":"clear"}),
                            "keyring" | "session" => json!({"kind":"replace","value":secret}),
                            _ => json!({"kind":"keep"}),
                        };
                        self.request("direct_save", "profile.save", json!({"profile":profile,"secret_update":update,"save_secret":action=="keyring"}));
                    }
                }
                _ => {
                    if self.cursor >= 2 && !modifiers.contains(KeyModifiers::CONTROL) {
                        self.form.edit(self.cursor, code);
                    }
                }
            },
            Screen::Settings => match code {
                KeyCode::Esc => self.navigate(Screen::Home),
                KeyCode::F(2) => match forms::settings_value(&self.form, &self.settings) {
                    Ok(v) => self.request("settings_save", "settings.set", json!({"settings":v})),
                    Err(e) => self.status = e,
                },
                _ => {
                    if !modifiers.contains(KeyModifiers::CONTROL) {
                        self.form.edit(self.cursor, code);
                    }
                }
            },
            Screen::Rules => match code {
                KeyCode::Esc => self.navigate(Screen::Home),
                KeyCode::F(4) => forms::add_rule(&mut self.form),
                KeyCode::F(2) => {
                    if let Some(p) = &self.selected_profile {
                        let redis = p["provider"] == "redis";
                        let rules: Vec<_> = self.form.fields.iter().filter(|f| f.key != "__add").map(|f| json!({"profile_id":p["id"],"scope_kind":if redis { "instance" } else { "database" },"scope_key":f.key,"level":f.value})).collect();
                        self.request("rules_save", "profile.rules.set", json!({"profile_id":p["id"],"expected_revision":self.rules_revision,"rules":rules}));
                    }
                }
                _ => {
                    if !modifiers.contains(KeyModifiers::CONTROL) {
                        self.form.edit(self.cursor, code);
                    }
                }
            },
            Screen::Servers => match code {
                KeyCode::Esc => self.navigate(Screen::Home),
                KeyCode::Char('n') => {
                    self.server = Value::Null;
                    self.auth = Value::Null;
                    self.form = forms::server(None);
                    self.baseline.clear();
                    self.desired.clear();
                    self.navigate(Screen::Server);
                    self.cursor = 2;
                }
                KeyCode::Enter => {
                    if let Some(s) = self.servers.get(self.cursor).cloned() {
                        self.open_server(s);
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(s) = self.servers.get(self.cursor).cloned() {
                        self.open_server(s);
                        self.confirm("archery.server.delete");
                    }
                }
                _ => {}
            },
            Screen::Server => match code {
                KeyCode::Esc => {
                    self.staged_credentials = None;
                    if !self.auth["auth_id"].is_null() {
                        self.request("auth_closed", "archery.auth.close", json!({"auth_id":self.auth["auth_id"],"controller_token":self.auth["controller_token"],"auth_generation":self.auth["auth_generation"]}));
                        self.auth = Value::Null;
                    }
                    self.request("servers", "archery.server.list", json!({}));
                }
                KeyCode::F(5) => self.login(),
                KeyCode::F(6) => {
                    let otp = self.form.take("otp");
                    self.request("auth", "archery.auth.verify", json!({"operation_id":Uuid::new_v4(),"auth_id":self.auth["auth_id"],"controller_token":self.auth["controller_token"],"auth_generation":self.auth["auth_generation"],"otp":otp}));
                }
                KeyCode::F(2) => self.save_server(),
                KeyCode::F(3) => self.request(
                    "instances",
                    "archery.instances",
                    json!({"operation_id":Uuid::new_v4(),"auth_id":self.auth["auth_id"]}),
                ),
                _ => {
                    if self.cursor >= 2 && !modifiers.contains(KeyModifiers::CONTROL) {
                        self.form.edit(self.cursor, code);
                    }
                }
            },
            Screen::Instances => match code {
                KeyCode::Esc => {
                    self.form = forms::server(Some(&self.server));
                    self.navigate(Screen::Server);
                }
                KeyCode::Char('p') => self.navigate(Screen::Preview),
                KeyCode::Char('d') => {
                    if let Some(i) = self.instances.get(self.cursor) {
                        self.desired
                            .retain(|p| p["connection"]["instance_id"] != i["id"]);
                    }
                }
                KeyCode::Char('n') => {
                    if let Some(i) = self.instances.get(self.cursor).cloned()
                        && let Some(p) = self
                            .desired
                            .iter()
                            .find(|p| p["connection"]["instance_id"] == i["id"])
                    {
                        self.form = forms::profile_name(p["name"].as_str().unwrap_or(""));
                        self.instance = i;
                        self.navigate(Screen::ProfileName);
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = self.instances.get(self.cursor).cloned() {
                        self.instance = i.clone();
                        let old = self
                            .desired
                            .iter()
                            .find(|p| p["connection"]["instance_id"] == i["id"]);
                        self.selected_databases = old
                            .and_then(|p| p["connection"]["databases"].as_array())
                            .map(|v| {
                                v.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.default_database = old
                            .and_then(|p| p["connection"]["default_database"].as_str())
                            .map(str::to_owned);
                        self.request("databases", "archery.databases", json!({"operation_id":Uuid::new_v4(),"auth_id":self.auth["auth_id"],"instance_id":i["id"]}));
                    }
                }
                _ => {}
            },
            Screen::Databases => match code {
                KeyCode::Esc => self.navigate(Screen::Instances),
                KeyCode::Char(' ') => {
                    if let Some(d) = self.databases.get(self.cursor).cloned() {
                        if self.selected_databases.contains(&d) {
                            self.selected_databases.retain(|v| v != &d);
                            if self.default_database.as_ref() == Some(&d) {
                                self.default_database = None;
                            }
                        } else {
                            self.selected_databases.push(d);
                        }
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(d) = self.databases.get(self.cursor).cloned()
                        && self.selected_databases.contains(&d)
                    {
                        self.default_database = Some(d);
                    }
                }
                KeyCode::Enter => self.select_databases(),
                _ => {}
            },
            Screen::ProfileName => match code {
                KeyCode::Esc => self.navigate(Screen::Instances),
                KeyCode::Enter => {
                    let name = self.form.get("name").to_owned();
                    if let Some(p) = self
                        .desired
                        .iter_mut()
                        .find(|p| p["connection"]["instance_id"] == self.instance["id"])
                    {
                        p["name"] = json!(name);
                    }
                    self.navigate(Screen::Instances);
                }
                _ => {
                    if !modifiers.contains(KeyModifiers::CONTROL) {
                        self.form.edit(0, code);
                    }
                }
            },
            Screen::Preview => match code {
                KeyCode::Esc => self.navigate(Screen::Instances),
                KeyCode::F(2) => self.confirm("archery.profile.apply_batch"),
                _ => {}
            },
            Screen::Cache => match code {
                KeyCode::Esc => self.navigate(Screen::Home),
                KeyCode::Char('d') => self.confirm("cache.retired.clear"),
                _ => {}
            },
            Screen::Confirm => match code {
                KeyCode::Esc | KeyCode::Char('n') => self.navigate(self.previous),
                KeyCode::Char('y') => {
                    let action = self.confirm.clone();
                    match action.as_str() { "profile.delete" => if let Some(p) = &self.selected_profile { self.request("delete", &action, json!({"profile_id":p["id"]})); }, "cache.retired.clear" => self.request("cache_clear", &action, json!({})), "archery.server.delete" => { let ids: Vec<_> = self.baseline.iter().map(|p| p["id"].clone()).collect(); self.request("server_deleted", &action, json!({"server_id":self.server["id"],"expected_revision":self.server["revision"],"expected_profile_ids":ids})); }, "archery.profile.apply_batch" => self.request("batch", &action, json!({"auth_id":self.auth["auth_id"],"server_id":self.server["id"],"expected_server_revision":self.server["revision"],"expected_profiles":self.baseline,"desired_profiles":self.desired})), _ => {} }
                }
                _ => {}
            },
        }
        false
    }
    fn open_server(&mut self, server: Value) {
        self.baseline = self
            .profiles
            .iter()
            .filter(|p| p["connection"]["server_id"] == server["id"])
            .cloned()
            .collect();
        self.desired = self.baseline.clone();
        self.form = forms::server(Some(&server));
        self.server = server;
        self.auth = Value::Null;
        self.staged_credentials = None;
        self.navigate(Screen::Server);
        self.cursor = 2;
    }
    fn login(&mut self) {
        let server = forms::server_value(&self.form);
        let password = self.form.take("password");
        let totp = self.form.take("totp");
        let password_action = self.form.get("password_action");
        let totp_action = self.form.get("totp_action");
        let update = |action: &str, value: &str| match action {
            "clear" => json!({"kind":"clear"}),
            "keyring" | "session" if !value.is_empty() => {
                json!({"kind":"replace","value":value,"persist":action=="keyring"})
            }
            _ => json!({"kind":"keep"}),
        };
        let password_update = update(password_action, &password);
        let totp_update = update(totp_action, &totp);
        let candidate = server["revision"] == 0
            || password_update["kind"] != "keep"
            || totp_update["kind"] != "keep"
            || server["http_tls"] != self.server["http_tls"]
            || server["totp_mode"] != self.server["totp_mode"];
        self.staged_credentials =
            Some(json!({"password_update":password_update,"totp_update":totp_update}));
        self.server = server.clone();
        self.request("auth", "archery.auth.begin", json!({"operation_id":Uuid::new_v4(),"expected_revision":server["revision"],"server":server,"password":if password.is_empty(){None}else{Some(password)},"totp_uri":if totp.is_empty(){None}else{Some(totp)},"candidate":candidate}));
    }
    fn save_server(&mut self) {
        let server = forms::server_value(&self.form);
        let updates = self.staged_credentials.take().unwrap_or_else(
            || json!({"password_update":{"kind":"keep"},"totp_update":{"kind":"keep"}}),
        );
        self.request("server_saved", "archery.server.save", json!({"server":server,"expected_revision":server["revision"],"auth_id":self.auth["auth_id"],"password_update":updates["password_update"],"totp_update":updates["totp_update"]}));
    }
    fn select_databases(&mut self) {
        if self.selected_databases.is_empty() {
            self.status = self
                .text("请选择至少一个数据库", "Select at least one database")
                .into();
            return;
        }
        let default = self
            .default_database
            .clone()
            .unwrap_or_else(|| self.selected_databases[0].clone());
        let old = self
            .desired
            .iter()
            .find(|p| p["connection"]["instance_id"] == self.instance["id"])
            .cloned();
        let mut p = old.unwrap_or_else(|| json!({"id":Uuid::new_v4(),"revision":1,"name":self.instance["name"],"provider":self.instance["provider"],"access_mode":"read_only","approval_policy":self.settings["approval"]["default_policy"],"connection":{"kind":"archery","server_id":self.server["id"],"instance_id":self.instance["id"],"instance_name":self.instance["name"]}}));
        p["connection"]["databases"] = json!(self.selected_databases);
        p["connection"]["default_database"] = json!(default);
        self.desired
            .retain(|p| p["connection"]["instance_id"] != self.instance["id"]);
        self.desired.push(p);
        self.navigate(Screen::Instances);
    }
}

//! TSP 路径选择器只枚举目录并回填输入，实际文件操作仍由原表单发起。

use super::*;
use crate::i18n::{UiArg, UiMessage, tr};
use std::path::{Component, Path};

pub(super) const METHOD: &str = "local.path-picker.read";
pub(super) const REPLY_PREFIX: &str = "local.path-picker/";
pub(super) const OPEN_PREFIX: &str = "browse-path:";
const ROW_LIMIT: usize = 256;
const NO_ROW: usize = usize::MAX;

#[cfg(test)]
#[path = "../tests/unit/path_picker.rs"]
mod tests;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    File,
    Multiple,
    Directory,
    Parent,
    Save,
}

impl Mode {
    fn parse(mode: &str) -> Option<Self> {
        Some(match mode {
            "file" => Self::File,
            "multiple" => Self::Multiple,
            "directory" => Self::Directory,
            "parent" => Self::Parent,
            "save" => Self::Save,
            _ => return None,
        })
    }

    fn title(self) -> &'static str {
        match self {
            Self::File => "选择文件",
            Self::Multiple => "选择多个文件",
            Self::Directory => "选择目录",
            Self::Parent => "选择父目录",
            Self::Save => "选择保存位置",
        }
    }

    fn folders_only(self) -> bool {
        matches!(self, Self::Directory | Self::Parent | Self::Save)
    }
}

pub(super) fn query_path(
    base: Option<&Path>,
    value: &str,
) -> std::result::Result<PathBuf, UiMessage> {
    let path = Path::new(value);
    let Some(base) = base.filter(|_| !path.is_absolute()) else {
        return Ok(path.to_owned());
    };
    if matches!(
        path.components().next(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        return Err(UiMessage::key("请输入普通相对路径或完整的绝对路径"));
    }
    Ok(base.join(path))
}

#[derive(Deserialize, Serialize)]
struct Browse {
    directory: Option<PathBuf>,
    initial: Option<PathBuf>,
    base: Option<PathBuf>,
    folders_only: bool,
    extensions: String,
    filter: String,
}

#[derive(Deserialize, Serialize)]
struct Entry {
    name: String,
    folder: bool,
}

#[derive(Deserialize, Serialize)]
struct Listing {
    directory: PathBuf,
    entries: Vec<Entry>,
    truncated: bool,
    skipped_names: bool,
    initial_is_dir: bool,
    error: Option<String>,
}

pub(super) async fn read_directory(params: Value) -> Result<Value> {
    let browse = serde_json::from_value(params)?;
    tokio::task::spawn_blocking(move || {
        serde_json::to_value(list_directory(browse)?).map_err(Into::into)
    })
    .await
    .map_err(|error| AppError::new("path_picker", error.to_string()))?
}

fn list_directory(browse: Browse) -> Result<Listing> {
    let initial_is_dir = browse.initial.as_deref().is_some_and(Path::is_dir);
    let directory = browse
        .directory
        .or_else(|| {
            let initial = browse.initial.as_deref().and_then(|path| {
                if initial_is_dir {
                    Some(path)
                } else {
                    path.parent()
                }
            });
            initial
                .filter(|path| path.is_absolute() && path.is_dir())
                .map(Path::to_owned)
                .or_else(|| {
                    browse
                        .base
                        .filter(|path| path.is_absolute() && path.is_dir())
                })
                .or_else(|| std::env::home_dir().filter(|path| path.is_absolute()))
        })
        .ok_or_else(|| AppError::new("path_picker_home", "无法确定本机用户主目录"))?;
    if directory.to_str().is_none() {
        return Err(AppError::new(
            "path_picker_encoding",
            "路径无法用 UTF-8 无损表示",
        ));
    }
    let mut listing = Listing {
        directory,
        entries: Vec::new(),
        truncated: false,
        skipped_names: false,
        initial_is_dir,
        error: None,
    };
    if let Err(error) = scan_directory(
        &browse.extensions,
        browse.folders_only,
        &browse.filter,
        &mut listing,
    ) {
        listing.entries.clear();
        listing.error = Some(error.to_string());
    }
    Ok(listing)
}

fn scan_directory(
    extensions: &str,
    folders_only: bool,
    filter: &str,
    listing: &mut Listing,
) -> std::io::Result<()> {
    let filter = filter.trim().to_lowercase();
    for entry in std::fs::read_dir(&listing.directory)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            listing.skipped_names = true;
            continue;
        };
        if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
            continue;
        }
        let mut kind = entry.file_type()?;
        if kind.is_symlink() {
            // 允许进入本机目录的符号链接；断链及目标不可访问时保留实际目录错误的入口。
            match std::fs::metadata(entry.path()) {
                Ok(metadata) => kind = metadata.file_type(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        let folder = kind.is_dir();
        if !folder
            && (folders_only
                || !kind.is_file()
                || (!extensions.is_empty()
                    && !Path::new(&name)
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            extensions
                                .split(',')
                                .any(|allowed| ext.eq_ignore_ascii_case(allowed))
                        })))
        {
            continue;
        }
        if listing.entries.len() == ROW_LIMIT {
            listing.truncated = true;
            break;
        }
        listing.entries.push(Entry { name, folder });
    }
    listing
        .entries
        .sort_unstable_by(|a, b| (!a.folder, &a.name).cmp(&(!b.folder, &b.name)));
    Ok(())
}

#[derive(Eq, PartialEq)]
struct Context {
    tab: Option<Uuid>,
    overlay: Option<String>,
    view: String,
    project_form: String,
    project_section: String,
    transfer_stage: String,
}

pub(super) struct Picker {
    key: String,
    context: Context,
    mode: Mode,
    field: String,
    extensions: String,
    original: String,
    directory: Option<PathBuf>,
    filter: Editor,
    listing: Option<Listing>,
    listed_request: Option<Uuid>,
    pending: Option<Uuid>,
    rescan: bool,
    error: Option<UiMessage>,
    selected: usize,
    marked: Vec<PathBuf>,
}

impl Picker {
    fn request(&mut self, backend: &Backend, base: Option<&Path>) {
        if self.pending.is_some() {
            // 目录读取可能触及慢磁盘，始终只让同一个 picker 有一条在途读取。
            self.rescan = true;
            return;
        }
        let initial = if self.field == "path" && self.context.overlay.as_deref() == Some("file") {
            query_path(base, &self.original).ok()
        } else {
            Some(PathBuf::from(&self.original))
        };
        let request = Uuid::new_v4();
        self.error = None;
        match backend.request(
            format!("{REPLY_PREFIX}{request}"),
            METHOD,
            json!(Browse {
                directory: self.directory.clone(),
                initial: initial.filter(|path| path.is_absolute()),
                base: base.map(Path::to_owned),
                folders_only: self.mode.folders_only(),
                extensions: self.extensions.clone(),
                filter: self.filter.text.clone(),
            }),
        ) {
            Ok(()) => self.pending = Some(request),
            Err(error) => self.error = Some(read_error(error.to_string())),
        }
    }

    fn navigate(&mut self, directory: PathBuf, backend: &Backend, base: Option<&Path>) {
        self.directory = Some(directory);
        self.filter = Editor::default();
        self.selected = NO_ROW;
        self.listing = None;
        self.request(backend, base);
    }

    fn current(&self) -> Option<&Listing> {
        self.pending
            .is_none()
            .then_some(self.listing.as_ref())
            .flatten()
    }

    fn entry(&self) -> Option<(&Path, &Entry)> {
        let listing = self.current()?;
        Some((&listing.directory, listing.entries.get(self.selected)?))
    }

    fn toggle_mark(&mut self) {
        let Some((directory, entry)) = self.entry().filter(|(_, entry)| !entry.folder) else {
            return;
        };
        let path = directory.join(&entry.name);
        if let Some(index) = self.marked.iter().position(|marked| marked == &path) {
            self.marked.remove(index);
        } else {
            self.marked.push(path);
        }
    }

    fn choose(&self) -> Option<Vec<PathBuf>> {
        if self.mode == Mode::Multiple {
            return (!self.marked.is_empty()).then(|| self.marked.clone());
        }
        let listing = self.current().filter(|listing| listing.error.is_none())?;
        if self.mode == Mode::File {
            let (dir, entry) = self.entry().filter(|(_, entry)| !entry.folder)?;
            return Some(vec![dir.join(&entry.name)]);
        }
        let mut directory = self.entry().filter(|(_, entry)| entry.folder).map_or_else(
            || listing.directory.clone(),
            |(dir, entry)| dir.join(&entry.name),
        );
        if matches!(self.mode, Mode::Save | Mode::Parent) {
            let name = (!listing.initial_is_dir)
                .then(|| Path::new(&self.original).file_name())
                .flatten();
            // 末尾分隔符让尚未命名的目标停留在原表单补写文件名或新目录名。
            directory.push(name.unwrap_or_default());
        }
        Some(vec![directory])
    }

    fn view(&self) -> Node<Msg> {
        let listing = self.current();
        let entries = listing.map_or(&[][..], |listing| listing.entries.as_slice());
        let mut items: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let mut item = ui::PickerItem::new(self.item_id(index), entry.name.clone());
                item.icon = Some(if entry.folder { "folder" } else { "file" }.into());
                item.title = Some(entry.name.clone());
                item
            })
            .collect();
        for (show, id, label) in [
            (
                listing.is_some_and(|l| l.truncated),
                "limit",
                "目录条目过多，请缩小筛选范围",
            ),
            (
                listing.is_some_and(|l| l.skipped_names),
                "encoding",
                "部分名称无法用 UTF-8 无损表示",
            ),
        ] {
            if show {
                let mut item = ui::PickerItem::new(id, tr(label).into_owned());
                item.disabled = Some(Value::Bool(true));
                items.push(item);
            }
        }
        let directory = self
            .directory
            .as_deref()
            .or_else(|| listing.map(|l| l.directory.as_path()));
        let message = self
            .error
            .as_ref()
            .map(UiMessage::render)
            .map(|s| s.into_owned())
            .or_else(|| {
                listing.and_then(|l| {
                    l.error
                        .as_ref()
                        .map(|e| read_error(e.clone()).render().into_owned())
                })
            });
        let mut actions = vec![
            json!({"id":"up", "label":tr("上一级目录"), "keys":["alt+up"], "disabled":directory.and_then(Path::parent).is_none()}),
            json!({"id":"cancel", "label":tr("取消"), "keys":["esc"], "end":true}),
            json!({"id":"choose", "label":tr(if self.mode == Mode::Multiple { "添加选中的文件" } else { "使用所选路径" }), "keys":["ctrl+enter"], "primary":true, "disabled":!self.can_choose()}),
        ];
        if self.mode == Mode::Multiple {
            actions.insert(1, json!({ "id":"mark", "label":i18n::format_message("切换选中（已选 {count}）", &[("count", &self.marked.len().to_string())]), "keys":["ctrl+space"], "disabled":self.entry().is_none_or(|(_, entry)| entry.folder) }));
        }
        if message.is_some() {
            actions.insert(
                1,
                json!({"id":"retry", "label":tr("重试"), "keys":["enter"]}),
            );
        }
        let current = if self.marked.is_empty() {
            Vec::new()
        } else {
            directory
                .into_iter()
                .flat_map(|dir| {
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| {
                            !entry.folder && self.marked.contains(&dir.join(&entry.name))
                        })
                        .map(|(index, _)| self.item_id(index))
                })
                .collect()
        };
        ui::picker()
            .key(self.key.clone())
            .title(tr(self.mode.title()).into_owned())
            .icon("folder")
            .prop("subtitle", directory.and_then(Path::to_str))
            .prop("preview", "none")
            .placeholder(tr("按名称筛选").into_owned())
            .query(self.filter.text.clone())
            .cursor(self.filter.cursor)
            .items(items)
            .prop(
                "state",
                if self.pending.is_some() {
                    "loading"
                } else if message.is_some() {
                    "error"
                } else {
                    "ready"
                },
            )
            .prop("message", message)
            .empty(tr("当前目录没有可选项").into_owned())
            .prop(
                "selected",
                (self.selected < entries.len()).then(|| self.item_id(self.selected)),
            )
            .current(current)
            .prop("actions", actions)
            .into()
    }

    fn item_id(&self, index: usize) -> String {
        format!("{}.{}", self.listed_request.unwrap_or_default(), index)
    }

    fn can_choose(&self) -> bool {
        match self.mode {
            Mode::Multiple => !self.marked.is_empty(),
            Mode::File => self.entry().is_some_and(|(_, entry)| !entry.folder),
            _ => self
                .current()
                .is_some_and(|listing| listing.error.is_none()),
        }
    }
}

fn read_error(error: String) -> UiMessage {
    UiMessage::template("无法读取目录：{error}", vec![("error", UiArg::Raw(error))])
}

impl App {
    fn path_picker_context(&self) -> Context {
        Context {
            tab: self.tabs.get(self.active).map(|tab| tab.id),
            overlay: self.workbench.overlay.clone(),
            view: self.view.clone(),
            project_form: self.workbench.project.local_form.clone(),
            project_section: self.workbench.project.section.clone(),
            transfer_stage: self.workbench.transfer.stage.clone(),
        }
    }

    pub(super) fn open_path_picker(&mut self, action: &str, backend: &Backend) -> bool {
        if self.path_picker.is_some() || self.workspace_switch_pending() || self.tab_close.is_some()
        {
            return false;
        }
        let Some((mode, options)) = action
            .strip_prefix(OPEN_PREFIX)
            .and_then(|s| s.split_once(':'))
        else {
            return false;
        };
        let Some(mode) = Mode::parse(mode) else {
            return false;
        };
        let Some((extensions, field)) = options.split_once(':') else {
            return false;
        };
        if mode != Mode::Multiple && !self.modal_allows_field(field) {
            return false;
        }
        let original = if field == "advanced-file-path" {
            &self.advanced_files.path.text
        } else {
            self.field(field)
        };
        let mut picker = Picker {
            key: format!("datapier-path-{}", Uuid::new_v4()),
            context: self.path_picker_context(),
            mode,
            field: field.to_owned(),
            extensions: extensions.to_owned(),
            original: original.to_owned(),
            directory: None,
            filter: Editor::default(),
            listing: None,
            listed_request: None,
            pending: None,
            rescan: false,
            error: None,
            selected: NO_ROW,
            marked: Vec::new(),
        };
        picker.request(backend, self.query_base.as_deref());
        self.path_picker = Some(picker);
        // Picker 的搜索框由应用处理按键，先结束原 Input 的 native editing。
        self.workbench.focus_request = Some(String::new());
        true
    }

    pub(super) fn reconcile_path_picker(&mut self) {
        if self
            .path_picker
            .as_ref()
            .is_some_and(|picker| picker.context != self.path_picker_context())
        {
            self.path_picker = None;
        }
    }

    pub(super) fn path_picker_view(&self) -> Option<Node<Msg>> {
        self.path_picker.as_ref().map(Picker::view)
    }

    pub(super) fn path_picker_reply(
        &mut self,
        backend: &Backend,
        request: &str,
        result: Result<Value>,
    ) -> bool {
        self.reconcile_path_picker();
        let Some(picker) = &mut self.path_picker else {
            return false;
        };
        let Ok(request) = Uuid::parse_str(request) else {
            return false;
        };
        if picker.pending != Some(request) {
            return false;
        }
        picker.pending = None;
        match result.and_then(|value| serde_json::from_value::<Listing>(value).map_err(Into::into))
        {
            Ok(listing) => {
                if picker.directory.is_none() {
                    picker.directory = Some(listing.directory.clone());
                    let original = Path::new(&picker.original)
                        .file_name()
                        .and_then(|name| name.to_str());
                    picker.selected = listing
                        .entries
                        .iter()
                        .position(|item| {
                            Some(item.name.as_str()) == original && !listing.initial_is_dir
                        })
                        .unwrap_or(NO_ROW);
                }
                if picker.directory.as_ref() == Some(&listing.directory) {
                    picker.listed_request = Some(request);
                    picker.listing = Some(listing);
                }
            }
            Err(error) => {
                picker.error = Some(match error.code.as_str() {
                    "path_picker_home" => UiMessage::key("无法确定本机用户主目录"),
                    "path_picker_encoding" => UiMessage::key("路径无法用 UTF-8 无损表示"),
                    _ => read_error(error.to_string()),
                })
            }
        }
        if std::mem::take(&mut picker.rescan) {
            picker.request(backend, self.query_base.as_deref());
        }
        true
    }

    pub(super) fn path_picker_event(&mut self, event: &tern_sdk::Event, backend: &Backend) -> bool {
        let Some(picker) = &mut self.path_picker else {
            return false;
        };
        let act = match event {
            tern_sdk::Event::Select(event) | tern_sdk::Event::Activate(event)
                if event.id.strip_prefix("layer.") == Some(picker.key.as_str()) =>
            {
                let Some((generation, index)) = event.item.rsplit_once('.') else {
                    return true;
                };
                if Uuid::parse_str(generation).ok() != picker.listed_request {
                    return true;
                }
                let Ok(index) = index.parse::<usize>() else {
                    return true;
                };
                if picker
                    .current()
                    .is_none_or(|listing| index >= listing.entries.len())
                {
                    return true;
                }
                picker.selected = index;
                ""
            }
            tern_sdk::Event::Action(event)
                if event.id.strip_prefix("layer.") == Some(picker.key.as_str()) =>
            {
                event.act.as_str()
            }
            _ => return false,
        };
        if matches!(event, tern_sdk::Event::Activate(_)) {
            self.path_picker_action("activate", backend);
        } else if !act.is_empty() {
            self.path_picker_action(act, backend);
        }
        true
    }

    pub(super) fn path_picker_key(&mut self, key: &tern_sdk::Key, backend: &Backend) {
        let Some(picker) = &mut self.path_picker else {
            return;
        };
        match key.name.as_str() {
            "escape" => self.path_picker_action("cancel", backend),
            "up" if key.alt => self.path_picker_action("up", backend),
            "enter" if key.ctrl || key.meta => self.path_picker_action("choose", backend),
            "enter" => self.path_picker_action("activate", backend),
            "space" if key.ctrl || key.meta => self.path_picker_action("mark", backend),
            "up" | "down" => {
                if let Some(last) = picker
                    .current()
                    .and_then(|listing| listing.entries.len().checked_sub(1))
                {
                    picker.selected = match (key.name.as_str(), picker.selected) {
                        ("up", NO_ROW) => last,
                        ("up", index) => index.saturating_sub(1),
                        (_, NO_ROW) => 0,
                        (_, index) => (index + 1).min(last),
                    };
                }
            }
            _ => {
                let edit = match key.name.as_str() {
                    "backspace" => Some(EditKey::Backspace),
                    "delete" => Some(EditKey::Delete),
                    "left" => Some(EditKey::Left),
                    "right" => Some(EditKey::Right),
                    "home" => Some(EditKey::Home),
                    "end" => Some(EditKey::End),
                    "a" if key.ctrl || key.meta => Some(EditKey::SelectAll),
                    "paste" => key.text.as_deref().map(EditKey::Text),
                    _ => key.typed().map(EditKey::Text),
                };
                let text_before = picker.filter.text.clone();
                if let Some(edit) = edit {
                    picker.filter.key(edit, key.shift);
                    if picker.filter.text != text_before {
                        picker.selected = NO_ROW;
                        picker.request(backend, self.query_base.as_deref());
                    }
                }
            }
        }
    }

    fn path_picker_action(&mut self, action: &str, backend: &Backend) {
        self.reconcile_path_picker();
        let Some(picker) = &mut self.path_picker else {
            return;
        };
        let base = self.query_base.as_deref();
        match action {
            "up" => {
                if let Some(parent) = picker.directory.as_deref().and_then(Path::parent) {
                    picker.navigate(parent.to_owned(), backend, base);
                }
            }
            "clear" => {
                picker.filter = Editor::default();
                picker.selected = NO_ROW;
                picker.request(backend, base);
            }
            "retry" => picker.request(backend, base),
            "mark" if picker.mode == Mode::Multiple => picker.toggle_mark(),
            "activate" => {
                if let Some((dir, entry)) = picker.entry().filter(|(_, entry)| entry.folder) {
                    picker.navigate(dir.join(&entry.name), backend, base);
                } else if picker.mode == Mode::Multiple {
                    picker.toggle_mark();
                } else if picker.can_choose() {
                    self.path_picker_action("choose", backend);
                } else if picker.error.is_some()
                    || picker.current().is_some_and(|l| l.error.is_some())
                {
                    picker.request(backend, base);
                }
            }
            "cancel" => self.close_path_picker(),
            "choose" => {
                let Some(paths) = picker.choose() else { return };
                let field = picker.field.clone();
                if picker.mode == Mode::Multiple {
                    if let Err(message) = self.append_transfer_paths(paths) {
                        self.path_picker.as_mut().unwrap().error = Some(message);
                        return;
                    }
                } else if let Some(path) = paths
                    .into_iter()
                    .next()
                    .and_then(|p| p.into_os_string().into_string().ok())
                {
                    if field == "advanced-file-path" {
                        if !self.advanced_files.pending {
                            self.advanced_files.path = Editor::new(path);
                        }
                    } else {
                        self.field_set(&field, path);
                        self.changed_field(&field);
                    }
                }
                self.close_path_picker();
            }
            _ => {}
        }
    }

    fn close_path_picker(&mut self) {
        if let Some(picker) = self.path_picker.take() {
            self.workbench.focus_request = Some(format!("input-{}", picker.field));
        }
    }

    pub(super) fn path_picker_shortcut(&mut self, key: &tern_sdk::Key, backend: &Backend) -> bool {
        if key.name != "f4" || key.alt || key.ctrl || key.meta {
            return false;
        }
        fn collect<'a>(node: &'a Node<Msg>, field: &str, actions: &mut Vec<&'a str>) {
            if let Some(action) = node.props().get("key").and_then(Value::as_str)
                && action.starts_with(OPEN_PREFIX)
                && action
                    .rsplit_once(':')
                    .is_some_and(|(_, name)| name == field)
                && node.props().get("disabled").and_then(Value::as_bool) != Some(true)
            {
                actions.push(action);
            }
            for child in node.child_nodes() {
                collect(child, field, actions);
            }
        }
        let view = views::render_window(self, None);
        let mut actions = Vec::new();
        for root in view.regions().into_iter().flatten() {
            collect(root, &self.focus, &mut actions);
        }
        actions
            .get(usize::from(key.shift))
            .is_some_and(|action| self.open_path_picker(action, backend))
    }
}

pub(crate) fn browse_button(
    field: &str,
    mode: &str,
    extensions: &str,
    label: &str,
    disabled: bool,
) -> Node<Msg> {
    let action = format!("{OPEN_PREFIX}{mode}:{extensions}:{field}");
    let label = tr(label).into_owned();
    ui::button(label.clone())
        .key(action.clone())
        .aria(label)
        .variant(ui::ButtonVariant::Quiet)
        .disabled(disabled)
        .on_click(Msg::Action(action))
        .into()
}

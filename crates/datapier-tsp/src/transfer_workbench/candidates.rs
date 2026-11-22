use super::*;

impl App {
    pub(super) fn transfer_catalog_load(
        &mut self,
        backend: &Backend,
        side: &str,
        scope: &str,
        target: Option<Value>,
    ) {
        self.transfer_catalog_page(backend, side, scope, target, false);
    }
    pub(super) fn transfer_catalog_page(
        &mut self,
        backend: &Backend,
        side: &str,
        scope: &str,
        target: Option<Value>,
        more: bool,
    ) {
        let project = self.workbench.transfer.project_mode;
        let connection = self
            .transfer_value(&format!(
                "{side}-{}",
                if project { "alias" } else { "profile" }
            ))
            .to_owned();
        if connection.is_empty() {
            return;
        }
        let database = target
            .as_ref()
            .and_then(|target| target["database"].as_str())
            .filter(|database| !database.is_empty())
            .unwrap_or_else(|| self.transfer_value(&format!("{side}-database")))
            .to_owned();
        let redis_db = self
            .transfer_value(&format!("{side}-redis-db"))
            .parse::<u32>()
            .ok();
        let endpoint = if project {
            json!({"connection_alias":connection})
        } else {
            json!({"profile_id":connection,"database":if database.is_empty() { None } else { Some(database.clone()) },"redis_db":redis_db})
        };
        let kind = if scope == "objects" {
            target.as_ref().and_then(|target| target["kind"].as_str())
        } else {
            None
        };
        let key = if let Some(kind) = kind {
            format!("{side}:{scope}:{kind}:{database}")
        } else if let Some(name) = target.as_ref().and_then(|target| target["name"].as_str()) {
            format!("{side}:{scope}:{database}:{name}")
        } else {
            format!("{side}:{scope}")
        };
        let (cursor, session) = if more {
            let Some(entry) = self.workbench.transfer.catalog.get(&key) else {
                return;
            };
            if entry.token.is_some() {
                return;
            }
            (
                entry
                    .value
                    .as_ref()
                    .and_then(|value| value.get("cursor"))
                    .cloned(),
                entry.session.clone(),
            )
        } else {
            if let Some(session) = self
                .workbench
                .transfer
                .catalog
                .get(&key)
                .and_then(|entry| entry.session.clone())
            {
                let _ = backend.request(
                    "transfer-catalog-close",
                    "connection.close",
                    json!({"session_id":session}),
                );
            }
            (None, None)
        };
        let token = Uuid::new_v4();
        let entry = self
            .workbench
            .transfer
            .catalog
            .entry(key.clone())
            .or_default();
        if !more {
            *entry = TransferCatalog::default();
        }
        entry.token = Some(token);
        entry.error = None;
        let params = json!({"endpoint":endpoint,"provider":self.transfer_value("provider"),"scope":scope,
            "target":if scope == "objects" { None } else { target.as_ref() },"kind":kind,
            "cursor":cursor,"session_id":session,
            "project_id":if project { self.workbench.project.project.as_ref().map(|project| &project["id"]) } else { None },
            "binding_group":self.workbench.project.binding_group});
        let tag = format!(
            "{}:{}:transfer-catalog/{token}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(tag, "transfer.catalog", params) {
            let entry = self.workbench.transfer.catalog.get_mut(&key).unwrap();
            entry.token = None;
            entry.error = Some(error.to_string());
        }
    }
    pub(crate) fn transfer_input_params(&self, i: usize) -> std::result::Result<Value, String> {
        let get = |part: &str| self.transfer_value(&format!("file-{part}-{i}"));
        let path = self.transfer_value(&format!("file-{i}"));
        if path.trim().is_empty() {
            return Err("请选择输入文件".into());
        }
        let columns: Vec<String> =
            serde_json::from_str(get("columns")).map_err(|e| e.to_string())?;
        let binary: Vec<String> =
            serde_json::from_str(get("binary-columns")).map_err(|e| e.to_string())?;
        let format = if self.transfer_value("mode") == "sql_restore" {
            "sql"
        } else {
            get("format")
        };
        let input = json!({"path":path,"format":format,"csv":{"encoding":get("encoding"),"delimiter":get("delimiter"),"header":get("header")=="true","columns":columns,"null_marker":get("null-marker"),"backslash_escape":get("backslash")=="true","binary_columns":binary}});
        let endpoint = self
            .workbench
            .transfer
            .project_mode
            .then(|| json!({"connection_alias":self.transfer_value("destination-alias")}));
        Ok(
            json!({"input":input,"provider":self.transfer_value("provider"),"endpoint":endpoint,
            "project_id":if self.workbench.transfer.project_mode { self.workbench.project.project.as_ref().map(|project| &project["id"]) } else { None },
            "binding_group":self.workbench.project.binding_group}),
        )
    }
    pub(super) fn transfer_input_load(&mut self, backend: &Backend, i: usize) {
        let key = format!("input:{i}");
        let params = match self.transfer_input_params(i) {
            Ok(params) => params,
            Err(error) => {
                self.workbench.transfer.catalog.insert(
                    key,
                    TransferCatalog {
                        error: Some(error),
                        ..Default::default()
                    },
                );
                return;
            }
        };
        let token = Uuid::new_v4();
        self.workbench.transfer.catalog.insert(
            key.clone(),
            TransferCatalog {
                token: Some(token),
                signature: params.clone(),
                ..Default::default()
            },
        );
        let tag = format!(
            "{}:{}:transfer-input/{token}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(tag, "transfer.input.inspect", params) {
            let entry = self.workbench.transfer.catalog.get_mut(&key).unwrap();
            entry.token = None;
            entry.error = Some(error.to_string());
        }
    }
}

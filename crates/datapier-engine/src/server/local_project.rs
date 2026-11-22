//! 本机项目 RPC 与目录身份解析。
use super::*;
use datapier_core::model::LocalProject;
use std::path::Path;

impl Engine {
    pub(super) async fn local_project_rpc(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "local_project.list" => Ok(json!({"projects":self.store.local_project_list().await?})),
            "local_project.get" => Ok(
                json!({"project":self.store.local_project_get(model::param(&params, "project_id")?).await?}),
            ),
            "local_project.save" => {
                let mut project: LocalProject = model::param(&params, "project")?;
                let expected: Option<u64> = optional(&params, "expected_revision")?;
                for root in &mut project.roots {
                    let path = std::fs::canonicalize(&root.path)?;
                    if !path.is_dir() {
                        return Err(AppError::invalid("仓库路径不是目录"));
                    }
                    root.path = path
                        .to_str()
                        .ok_or_else(|| AppError::invalid("仓库路径不是 UTF-8"))?
                        .to_owned();
                    let identity = super::project::binding_root_identity(&root.path)?;
                    // 已登记的目录不能因为原路径被替换而自动获得新身份。
                    if !root.identity.is_empty() && root.identity != identity {
                        return Err(AppError::new(
                            "conflict",
                            "仓库目录身份已变化，请显式重新关联",
                        ));
                    }
                    root.identity = identity;
                }
                let saved = self.store.local_project_save(&project, expected).await?;
                self.emit_local_project_changed(saved.id, saved.revision);
                Ok(json!({"project":saved}))
            }
            "local_project.delete" => {
                let id = model::param(&params, "project_id")?;
                let revision: u64 = model::param(&params, "expected_revision")?;
                self.store.local_project_delete(id, revision).await?;
                self.emit_local_project_changed(id, revision.saturating_add(1));
                Ok(json!({}))
            }
            "local_project.resolve" => {
                let path: String = model::param(&params, "path")?;
                let host: Option<String> = optional(&params, "host")?;
                if host.as_deref().is_some_and(|h| {
                    !h.is_empty()
                        && h != "localhost"
                        && std::env::var("HOSTNAME").ok().as_deref() != Some(h)
                        && std::env::var("COMPUTERNAME").ok().as_deref() != Some(h)
                }) {
                    return Ok(json!({"project":null,"root":null,"status":"remote"}));
                }
                let path = match std::fs::canonicalize(path) {
                    Ok(p) if p.is_dir() => p,
                    _ => return Ok(json!({"project":null,"root":null,"status":"unavailable"})),
                };
                let projects = self.store.local_project_list().await?;
                let mut best = None;
                for project in &projects {
                    for root in &project.roots {
                        let candidate = Path::new(&root.path);
                        if path.starts_with(candidate)
                            && best.as_ref().is_none_or(
                                |(_, r): &(
                                    &LocalProject,
                                    &datapier_core::model::LocalProjectRoot,
                                )| {
                                    candidate.components().count()
                                        > Path::new(&r.path).components().count()
                                },
                            )
                        {
                            best = Some((project, root));
                        }
                    }
                }
                match best {
                    Some((project, root))
                        if super::project::binding_root_identity(&root.path)
                            .ok()
                            .as_deref()
                            == Some(root.identity.as_str()) =>
                    {
                        Ok(json!({"project":project,"root":root.path,"status":"matched"}))
                    }
                    Some((_, root)) => {
                        Ok(json!({"project":null,"root":root.path,"status":"identity_changed"}))
                    }
                    None => Ok(json!({"project":null,"root":null,"status":"unassociated"})),
                }
            }
            _ => Err(AppError::new("not_found", "未知本机项目方法")),
        }
    }
    fn emit_local_project_changed(&self, id: Uuid, revision: u64) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "local_project.changed".into(),
            task_id: Uuid::nil(),
            payload: json!({"project_id":id,"revision":revision}),
        };
        for sender in state.event_senders.values() {
            let _ = sender.send(event.clone());
        }
    }
}

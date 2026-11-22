use super::super::transfer;
use super::*;

impl Engine {
    pub(super) async fn project_artifact_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        ws: String,
    ) -> Result<Value> {
        match method {
            "project.export" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let path: PathBuf = model::param(&params, "path")?;
                let slot = self.project_slot(owner, &ws, id)?;
                let lease = self.reserve_project(owner, WORK_BYTES)?;
                let snapshot = slot.lock().await.snapshot.clone();
                blocking(move || {
                    let _lease = lease;
                    project::export_archive(&snapshot, &path)
                })
                .await?;
                Ok(json!({"state":"published"}))
            }
            "project.import" => {
                let archive: PathBuf = model::param(&params, "path")?;
                let destination: PathBuf = model::param(&params, "destination")?;
                let project_lease = self.reserve_project(owner, PROJECT_BYTES)?;
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let (stage_id, preparing) = self
                    .begin_project_stage(
                        owner,
                        &root,
                        &destination.to_string_lossy(),
                        "import",
                        None,
                    )
                    .await?;
                let staging = root.clone();
                let worker_lease = work_lease.clone();
                let store = self.store.clone();
                let mut journal_identity = preparing.identity.clone();
                let artifact_id = preparing.id;
                let expected_revision = preparing.revision;
                let runtime = tokio::runtime::Handle::current();
                let stage = blocking(move || {
                    let _lease = worker_lease;
                    project::prepare_import_in(
                        &archive,
                        &destination,
                        &staging,
                        stage_id,
                        true,
                        |stage| {
                            let bytes = serde_json::to_vec(stage)?;
                            journal_identity.sha256 = Some(digest_bytes(&bytes));
                            journal_identity.canonical_path =
                                Some(stage.destination.clone().into());
                            journal_identity.ownership_digest =
                                stage.digests.get("datapier.project.json").cloned();
                            runtime.block_on(store.transfer_update_artifact(
                                artifact_id,
                                owner,
                                Some(expected_revision),
                                "preparing",
                                Some(&serde_json::to_value(journal_identity)?),
                            ))?;
                            Ok(())
                        },
                    )
                })
                .await?;
                let preparing = self.store.transfer_artifact(preparing.id).await?;
                let mut ready_identity = preparing.identity.clone();
                ready_identity.kind = Some("import".into());
                self.store
                    .transfer_update_artifact(
                        preparing.id,
                        owner,
                        Some(preparing.revision),
                        "ready",
                        Some(&serde_json::to_value(ready_identity)?),
                    )
                    .await?;
                let artifact = self.store.transfer_artifact(preparing.id).await?;
                drop(_stage_guard);
                if self.state.lock().unwrap().projects.projects.contains_key(&(
                    owner,
                    ws.clone(),
                    stage.project_id,
                )) {
                    return Err(AppError::new(
                        "busy",
                        "项目已打开；导入 stage 已保留，请先关闭项目",
                    ));
                }
                let gate = self.artifact_gate(artifact.id);
                let _guard = gate.lock().await;
                owned(
                    &self.store.transfer_artifact(artifact.id).await?,
                    owner,
                    artifact.revision,
                )?;
                let stage_id = stage.id;
                let worker_lease = work_lease.clone();
                let result = blocking(move || {
                    let _lease = worker_lease;
                    project::commit_import(&root, stage_id)
                })
                .await;
                match result {
                    Ok(snapshot) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        let response = snapshot_json(&snapshot);
                        let memory = Arc::new(
                            transfer::MemoryReservation::reserve(
                                &self.store,
                                owner,
                                datapier_core::execution::migration::encoded_size(&snapshot)?,
                            )
                            .await?,
                        );
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new(
                                "disconnected",
                                "导入已发布，但客户端已断开",
                            ));
                        }
                        let key = (owner, ws, snapshot.manifest.id);
                        if state.projects.projects.contains_key(&key) {
                            return Err(AppError::new(
                                "busy",
                                "导入已发布，项目已在当前工作区打开",
                            ));
                        }
                        state.projects.projects.insert(
                            key,
                            Arc::new(AsyncMutex::new(ProjectSlot {
                                snapshot: Arc::new(ChargedSnapshot {
                                    snapshot,
                                    _memory: memory,
                                }),
                                restored_baseline: false,
                                _lease: project_lease,
                            })),
                        );
                        Ok(response)
                    }
                    Err(error) => Err(error),
                }
            }
            "project.stages.discover" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let scan_root = root.clone();
                let (saves, imports, conflicts) = blocking_leased(work_lease.clone(), move || {
                    let mut saves = Vec::new();
                    let mut imports = Vec::new();
                    let mut conflicts = Vec::new();
                    for entry in std::fs::read_dir(&scan_root)? {
                        let entry = entry?;
                        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                        let Ok(id) = Uuid::parse_str(&name) else { continue };
                        let dir = entry.path();
                        if dir.join("import.json").exists() {
                            match project::read_import_journal(&scan_root, id) {
                                Ok(stage) => imports.push(stage),
                                Err(error) => conflicts.push(json!({"stage_id":id,"path":dir,"file":"import.json","code":error.code,"message":error.message})),
                            }
                        } else if dir.join("stage.json").exists() {
                            match project::read_stage(&scan_root, id) {
                                Ok(stage) => saves.push(stage.identity),
                                Err(error) => conflicts.push(json!({"stage_id":id,"path":dir,"file":"stage.json","code":error.code,"message":error.message})),
                            }
                        }
                    }
                    Ok((saves, imports, conflicts))
                }).await?;
                let mut known = BTreeSet::new();
                let mut offset = 0;
                loop {
                    let page = self.store.transfer_artifacts(None, offset, 100).await?;
                    if page.is_empty() {
                        break;
                    }
                    offset += page.len() as u64;
                    for artifact in page {
                        if artifact.kind == "project_stage"
                            && let Some(id) = artifact.identity.file_id
                        {
                            known.insert(id);
                        }
                    }
                }
                let mut registered = Vec::new();
                for identity in saves {
                    if known.contains(&identity.id.to_string()) {
                        continue;
                    }
                    known.insert(identity.id.to_string());
                    let stage_root = root.clone();
                    let id = identity.id;
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_stage(&stage_root, id)
                    })
                    .await?;
                    registered.push(artifact_view(
                        &self
                            .register_project_stage(
                                owner,
                                &identity.stage_dir,
                                &identity.root,
                                id,
                                "save",
                                &stage.preview.digest,
                            )
                            .await?,
                    ));
                }
                for stage in imports {
                    if known.contains(&stage.id.to_string()) {
                        continue;
                    }
                    known.insert(stage.id.to_string());
                    let digest = stage
                        .digests
                        .get("datapier.project.json")
                        .ok_or_else(|| AppError::invalid("导入 stage 缺少 manifest 摘要"))?;
                    let check_root = root.clone();
                    let stage_id = stage.id;
                    let complete = blocking_leased(work_lease.clone(), move || {
                        Ok(project::read_import_stage(&check_root, stage_id).is_ok())
                    })
                    .await?;
                    if complete {
                        registered.push(artifact_view(
                            &self
                                .register_project_stage(
                                    owner,
                                    &stage.stage_dir,
                                    &stage.destination,
                                    stage.id,
                                    "import",
                                    digest,
                                )
                                .await?,
                        ));
                    } else {
                        let identity = ArtifactIdentity {
                            path: Some(stage.stage_dir.clone().into()),
                            canonical_path: Some(stage.destination.clone().into()),
                            file_id: Some(stage.id.to_string()),
                            kind: Some("import_preparing".into()),
                            stage_dir_identity: Some(binding_root_identity(&stage.stage_dir)?),
                            ownership_digest: Some(digest.clone()),
                            sha256: Some(digest_bytes(&serde_json::to_vec(&stage)?)),
                            ..Default::default()
                        };
                        let id = self
                            .store
                            .transfer_register_artifact(
                                owner,
                                None,
                                "project_stage",
                                &serde_json::to_value(identity)?,
                            )
                            .await?;
                        registered.push(artifact_view(&self.store.transfer_artifact(id).await?));
                    }
                }
                let scan_root = root.clone();
                let names = blocking_leased(work_lease.clone(), move || -> Result<Vec<String>> {
                    let mut names = Vec::new();
                    for entry in std::fs::read_dir(scan_root)? {
                        let entry = entry?;
                        if let Some(name) = entry.file_name().to_str()
                            && Uuid::parse_str(name).is_ok()
                        {
                            names.push(name.to_owned());
                        }
                    }
                    Ok(names)
                })
                .await?;
                for name in names {
                    if known.contains(&name) {
                        continue;
                    }
                    let identity = ArtifactIdentity {
                        path: Some(root.join(&name)),
                        file_id: Some(name),
                        kind: Some("orphan_preparing".into()),
                        ..Default::default()
                    };
                    let id = self
                        .store
                        .transfer_register_artifact(
                            owner,
                            None,
                            "project_stage",
                            &serde_json::to_value(identity)?,
                        )
                        .await?;
                    registered.push(artifact_view(&self.store.transfer_artifact(id).await?));
                }
                Ok(json!({"artifacts":registered,"conflicts":conflicts}))
            }
            "transfer.artifact.list" => {
                let offset = optional(&params, "offset")?.unwrap_or(0u64);
                let limit = optional(&params, "limit")?.unwrap_or(100u64);
                let artifacts = self.store.transfer_artifacts(None, offset, limit).await?;
                let next_offset = offset + artifacts.len() as u64;
                let next = if !artifacts.is_empty()
                    && !self
                        .store
                        .transfer_artifacts(None, next_offset, 1)
                        .await?
                        .is_empty()
                {
                    Some(next_offset)
                } else {
                    None
                };
                Ok(
                    json!({"artifacts":artifacts.iter().map(artifact_view).collect::<Vec<_>>(),"next":next}),
                )
            }
            "transfer.artifact.get" => {
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                if artifact.revision != revision {
                    return Err(AppError::new("conflict", "资源版本已变化"));
                }
                let running = self.artifact_running(&artifact);
                if running {
                    return Err(AppError::new("busy", "资源关联任务仍在运行"));
                }
                let active = artifact
                    .owner_client_id
                    .is_some_and(|client| self.state.lock().unwrap().clients.contains(&client));
                if active && artifact.owner_client_id != Some(owner) {
                    return Err(AppError::new("busy", "资源所有者仍活跃"));
                }
                let mut response = json!({"artifact":artifact_view(&artifact)});
                if !active && growing_partial(&artifact) {
                    let identity = artifact.identity.clone();
                    response["observed_identity"] =
                        serde_json::to_value(blocking(move || inspect_partial(&identity)).await?)?;
                    response["requires_partial_confirmation"] = json!(true);
                }
                Ok(response)
            }
            "transfer.artifact.claim" => {
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                // 客户端租约结束不代表已发送的数据库操作终止；writer 收尾前保持原归属。
                let running = self.artifact_running(&artifact);
                if running {
                    return Err(AppError::new("busy", "资源关联任务仍在运行，不能领取"));
                }
                let active = artifact
                    .owner_client_id
                    .is_some_and(|client| self.state.lock().unwrap().clients.contains(&client));
                let observed = if growing_partial(&artifact) {
                    if active {
                        return Err(AppError::new("busy", "文件仍由活跃客户端处理"));
                    }
                    let expected: Option<String> = optional(&params, "confirm_partial_sha256")?;
                    let expected = expected.ok_or_else(|| {
                        AppError::new("confirmation_required", "请先查看并确认当前未完成文件")
                    })?;
                    let identity = artifact.identity.clone();
                    let observed = blocking(move || inspect_partial(&identity)).await?;
                    if observed.sha256.as_deref() != Some(expected.as_str()) {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "未完成文件在确认后又发生变化",
                        ));
                    }
                    Some(observed)
                } else {
                    None
                };
                let claimed = self
                    .store
                    .transfer_claim_artifact(id, owner, revision, artifact.owner_client_id, active)
                    .await?;
                if let Some(identity) = observed {
                    self.store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(claimed),
                            "cleanup_pending",
                            Some(&serde_json::to_value(identity)?),
                        )
                        .await?;
                }
                Ok(json!({"artifact":artifact_view(&self.store.transfer_artifact(id).await?)}))
            }
            "project.recovery.read" | "project.recovery.prepare" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                owned(&artifact, owner, revision)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let stage_id = registered_stage(&artifact, &root)?;
                if artifact
                    .identity
                    .kind
                    .as_deref()
                    .is_some_and(|kind| kind.ends_with("_preparing"))
                {
                    if method != "project.recovery.read" {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "stage 尚未完整准备，只能查看或清理",
                        ));
                    }
                    let path = root.join(stage_id.to_string());
                    let (entries, observed_identity, journal_sha256) = blocking_leased(work_lease.clone(), move || -> Result<(Vec<Value>, Option<String>, Option<String>)> {
                        match std::fs::symlink_metadata(&path) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None, None)),
                            Err(e) => return Err(e.into()),
                            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Err(AppError::new("recovery_conflict", "stage 目录已被替换")),
                            Ok(_) => {}
                        }
                        let identity = binding_root_identity(path.to_str().ok_or_else(|| AppError::invalid("stage 路径不是 UTF-8"))?)?;
                        let journal = path.join("import.json");
                        let journal_digest = match std::fs::symlink_metadata(&journal) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(e) => return Err(e.into()),
                            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MIB as u64 => None,
                            Ok(_) => Some(digest_bytes(&std::fs::read(journal)?)),
                        };
                        let mut entries = Vec::new();
                        for entry in std::fs::read_dir(path)? {
                            let entry = entry?;
                            let entry_path = entry.path();
                            let meta = std::fs::symlink_metadata(&entry_path)?;
                            let kind = if meta.file_type().is_symlink() { "symlink" } else if meta.is_file() { "file" } else if meta.is_dir() { "directory" } else { "other" };
                            entries.push(json!({"path":entry.file_name().to_string_lossy(),"type":kind,"size":meta.len(),"file_id":stage_entry_identity(&entry_path, &meta)}));
                        }
                        entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                        Ok((entries, Some(identity), journal_digest))
                    }).await?;
                    return Ok(
                        json!({"id":id,"revision":revision,"state":"preparing","root":artifact.identity.canonical_path,"files":entries,"directory_identity_bound":artifact.identity.stage_dir_identity.is_some(),"observed_directory_identity":observed_identity,"journal_sha256":journal_sha256}),
                    );
                }
                if artifact.identity.kind.as_deref() == Some("import") {
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_import_stage(&root, stage_id)
                    })
                    .await?;
                    let stored = stage
                        .digests
                        .get("datapier.project.json")
                        .ok_or_else(|| AppError::invalid("导入 stage 缺少 manifest 摘要"))?;
                    if artifact.identity.ownership_digest.as_ref() != Some(stored)
                        || artifact.identity.canonical_path.as_deref()
                            != Some(Path::new(&stage.destination))
                    {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "导入 stage 身份与登记不一致",
                        ));
                    }
                    let files: Vec<_> = stage.digests.iter().map(|(path, digest)| json!({"path":path,"old_digest":null,"new_digest":digest,"current_digest":null,"status":"staged"})).collect();
                    if method == "project.recovery.read" {
                        return Ok(
                            json!({"id":id,"revision":revision,"project_id":stage.project_id,"state":artifact.state,"root":stage.destination,"files":files}),
                        );
                    }
                    let action: ProjectRecoveryAction = model::param(&params, "action")?;
                    let plan_id = Uuid::new_v4();
                    let digest =
                        digest_bytes(&serde_json::to_vec(&(id, revision, stored, &action))?);
                    {
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new("disconnected", "客户端会话已关闭"));
                        }
                        state.projects.insert_recovery(
                            plan_id,
                            RecoveryPlan {
                                owner,
                                artifact_id: id,
                                artifact_revision: revision,
                                digest: digest.clone(),
                                action,
                            },
                        )?;
                    }
                    return Ok(
                        json!({"plan_id":plan_id,"revision":1,"digest":digest,"changes":files}),
                    );
                }
                if artifact.identity.kind.as_deref() != Some("save") {
                    return Err(AppError::invalid("未知项目 stage 类型"));
                }
                let stage = blocking_leased(work_lease.clone(), move || {
                    project::read_stage(&root, stage_id)
                })
                .await?;
                if artifact.identity.ownership_digest.as_deref()
                    != Some(stage.preview.digest.as_str())
                    || artifact.identity.canonical_path.as_deref()
                        != Some(Path::new(&stage.identity.root))
                {
                    return Err(AppError::new("recovery_conflict", "stage 身份与登记不一致"));
                }
                if method == "project.recovery.prepare" {
                    let action: ProjectRecoveryAction = model::param(&params, "action")?;
                    let plan_id = Uuid::new_v4();
                    let digest = digest_bytes(&serde_json::to_vec(&(
                        id,
                        revision,
                        &stage.preview.digest,
                        &action,
                    ))?);
                    let changes = change_summaries(&stage.preview, false);
                    {
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new("disconnected", "客户端会话已关闭"));
                        }
                        state.projects.insert_recovery(
                            plan_id,
                            RecoveryPlan {
                                owner,
                                artifact_id: id,
                                artifact_revision: revision,
                                digest: digest.clone(),
                                action,
                            },
                        )?;
                    }
                    Ok(json!({"plan_id":plan_id,"revision":1,"digest":digest,"changes":changes}))
                } else {
                    let files = blocking_leased(work_lease.clone(), move || {
                        let mut files = Vec::new();
                        for change in &stage.preview.changes {
                            let (current, status) = match current_digest(Path::new(&stage.identity.root), &change.path) {
                                Ok(current) => { let status = if current == change.old_digest { "old" } else if current == change.new_digest { "new" } else { "conflict" }; (current, status) },
                                Err(error) if error.code == "recovery_conflict" => (None, "conflict"),
                                Err(error) => return Err(error),
                            };
                            files.push(json!({"path":change.path,"old_digest":change.old_digest,"new_digest":change.new_digest,"current_digest":current,"status":status}));
                        }
                        Ok(json!({"id":id,"revision":revision,"project_id":stage.identity.project_id,"state":stage.state,"root":stage.identity.root,"files":files}))
                    }).await?;
                    Ok(files)
                }
            }
            "project.recovery.commit" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let plan_id: Uuid = model::param(&params, "plan_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let digest: String = model::param(&params, "digest")?;
                let slot = {
                    let mut state = self.state.lock().unwrap();
                    state.projects.prune();
                    state
                        .projects
                        .recoveries
                        .get(&plan_id)
                        .map(|entry| entry.slot.clone())
                        .ok_or_else(|| AppError::new("not_found", "恢复计划不存在或已过期"))?
                };
                let plan = slot.lock().await;
                if plan.owner != owner || revision != 1 || digest != plan.digest {
                    return Err(AppError::new(
                        "conflict",
                        "恢复计划所有者、版本或摘要不匹配",
                    ));
                }
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.clients.contains(&owner)
                        || !state
                            .projects
                            .recoveries
                            .get(&plan_id)
                            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &slot))
                    {
                        return Err(AppError::new("conflict", "恢复计划已失效"));
                    }
                    state.projects.recoveries.remove(&plan_id);
                }
                let gate = self.artifact_gate(plan.artifact_id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(plan.artifact_id).await?;
                owned(&artifact, owner, plan.artifact_revision)?;
                let root = ensure_stage_root(stage_root(self)).await?;
                let stage_id = registered_stage(&artifact, &root)?;
                if artifact.identity.kind.as_deref() == Some("import") {
                    let root_for_check = root.clone();
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_import_stage(&root_for_check, stage_id)
                    })
                    .await?;
                    if artifact.identity.ownership_digest.as_ref()
                        != stage.digests.get("datapier.project.json")
                        || artifact.identity.canonical_path.as_deref()
                            != Some(Path::new(&stage.destination))
                    {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "导入 stage 身份与登记不一致",
                        ));
                    }
                    let project_id = stage.project_id;
                    let state = match plan.action.clone() {
                        ProjectRecoveryAction::Continue => {
                            blocking_leased(work_lease.clone(), move || {
                                project::commit_import(&root, stage_id)
                            })
                            .await?;
                            "published"
                        }
                        ProjectRecoveryAction::Rollback => {
                            blocking_leased(work_lease.clone(), move || cleanup_import(&stage))
                                .await?;
                            "cleaned"
                        }
                    };
                    self.store
                        .transfer_update_artifact(
                            artifact.id,
                            owner,
                            Some(artifact.revision),
                            state,
                            None,
                        )
                        .await?;
                    return Ok(json!({"state":state,"project_id":project_id}));
                }
                let root_for_check = root.clone();
                let stage = blocking_leased(work_lease.clone(), move || {
                    project::read_stage(&root_for_check, stage_id)
                })
                .await?;
                if artifact.identity.ownership_digest.as_deref()
                    != Some(stage.preview.digest.as_str())
                    || artifact.identity.canonical_path.as_deref()
                        != Some(Path::new(&stage.identity.root))
                {
                    return Err(AppError::new("recovery_conflict", "stage 身份与登记不一致"));
                }
                let action = plan.action.clone();
                let result = blocking_leased(work_lease.clone(), move || {
                    project::recover(&root, stage_id, action)
                })
                .await;
                match result {
                    Ok(stage) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        Ok(json!({"state":stage.state,"project_id":stage.identity.project_id}))
                    }
                    Err(error) => {
                        if error.code == "recovery_conflict" {
                            self.store
                                .transfer_update_artifact(
                                    artifact.id,
                                    owner,
                                    Some(artifact.revision),
                                    "recovery_conflict",
                                    None,
                                )
                                .await?;
                        }
                        Err(error)
                    }
                }
            }
            "transfer.artifact.export" | "transfer.artifact.cleanup" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                owned(&artifact, owner, revision)?;
                let stage_gate = (artifact.kind == "project_stage")
                    .then(|| self.project_binding_gate(Uuid::nil(), "project-stage-registration"));
                let _stage_guard = if let Some(gate) = &stage_gate {
                    Some(gate.lock().await)
                } else {
                    None
                };
                let running = self.artifact_running(&artifact);
                let live_writer = growing_partial(&artifact)
                    && artifact
                        .owner_client_id
                        .is_some_and(|id| self.state.lock().unwrap().clients.contains(&id));
                if running || live_writer {
                    return Err(AppError::new("busy", "资源仍在写入，不能导出或清理"));
                }
                if artifact.kind == "redis_temporary" {
                    if method == "transfer.artifact.export" {
                        return Err(AppError::new("unsupported", "不支持导出 Redis 临时对象"));
                    }
                    if matches!(
                        artifact.state.as_str(),
                        "published" | "outcome_unknown" | "cleaned"
                    ) {
                        return Err(AppError::invalid("已发布、已清理或结果未知的资源不能清理"));
                    }
                    let identity = datapier_core::providers::redis::RedisTemporaryIdentity::new(
                        &artifact.identity,
                    )?;
                    let profile_id = artifact.identity.profile_id.ok_or_else(|| {
                        AppError::new("recovery_conflict", "Redis 临时对象缺少连接配置")
                    })?;
                    let sessions: Vec<_> = self
                        .state
                        .lock()
                        .unwrap()
                        .databases
                        .values()
                        .filter(|database| database.profile.id == profile_id)
                        .cloned()
                        .collect();
                    for database in sessions {
                        let session = database
                            .session
                            .try_lock()
                            .map_err(|_| AppError::new("busy", "Redis 会话正在使用"))?;
                        if let ProviderSession::Redis(redis) = &*session
                            && redis.owns_temporary_artifact(id)
                        {
                            return Err(AppError::new("busy", "Redis 临时对象仍在写入"));
                        }
                    }
                    let redis_db = artifact.identity.redis_db.ok_or_else(|| {
                        AppError::new("recovery_conflict", "Redis 临时对象缺少数据库")
                    })?;
                    let profile_lock = self.profile_lock(profile_id);
                    let _profile_guard = profile_lock.lock().await;
                    let mut profile = self.store.profile(profile_id).await?;
                    if profile.provider != model::ProviderKind::Redis || profile.is_archery() {
                        return Err(AppError::new(
                            "permission_denied",
                            "清理需要直连 Redis 写入权限",
                        ));
                    }
                    profile.ensure_write()?;
                    if profile
                        .direct()?
                        .visible_databases
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(&redis_db.to_string()))
                    {
                        return Err(AppError::new(
                            "permission_denied",
                            "Redis 数据库不在可见范围",
                        ));
                    }
                    profile.direct_mut()?.redis_db = redis_db;
                    let secret = match optional::<String>(&params, "secret")? {
                        Some(secret) => Some(secret),
                        None => self.store.secret(profile_id).await?,
                    };
                    let timeout = Duration::from_secs(
                        self.store.settings().await?.limits.connect_timeout_seconds,
                    );
                    let mut session = tokio::time::timeout(
                        timeout,
                        datapier_core::providers::redis::RedisSession::connect(profile, secret),
                    )
                    .await
                    .map_err(|_| AppError::new("timeout", "连接超时"))??;
                    let pending_revision = self
                        .store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(revision),
                            "cleanup_pending",
                            None,
                        )
                        .await?;
                    // 提前登记发送事实；崩溃或丢失回包后，不允许自动重发。
                    let sent_revision = self
                        .store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(pending_revision),
                            "outcome_unknown",
                            None,
                        )
                        .await?;
                    let result =
                        tokio::time::timeout(timeout, session.cleanup_temporary(&identity)).await;
                    match result {
                        Ok(Ok(())) => {
                            // 仅本次 EVAL 已明确回包时，允许确认结果未知状态。
                            self.store
                                .transfer_confirm_redis_cleanup(id, owner, sent_revision, "cleaned")
                                .await?;
                            return Ok(json!({"state":"cleaned"}));
                        }
                        Ok(Err(error)) if error.code == "recovery_conflict" => {
                            self.store
                                .transfer_confirm_redis_cleanup(
                                    id,
                                    owner,
                                    sent_revision,
                                    "recovery_conflict",
                                )
                                .await?;
                            return Err(error);
                        }
                        Ok(Err(error)) => {
                            return Err(AppError::unknown("Redis 清理结果未知，请人工核对")
                                .with_details(
                                    json!({"cause_code":error.code,"cause_message":error.message}),
                                ));
                        }
                        Err(_) => {
                            return Err(AppError::unknown("Redis 清理超时，结果未知，请人工核对"));
                        }
                    }
                }
                if method == "transfer.artifact.cleanup"
                    && (artifact.state == "outcome_unknown"
                        || (artifact.state == "published" && artifact.kind != "project_stage"))
                {
                    return Err(AppError::invalid("已发布或结果未知的资源不能清理"));
                }
                let export_path: Option<PathBuf> = optional(&params, "path")?;
                if method == "transfer.artifact.export" && export_path.is_none() {
                    return Err(AppError::invalid("缺少导出目标"));
                }
                let cleanup_revision = if method == "transfer.artifact.cleanup" {
                    Some(
                        self.store
                            .transfer_update_artifact(
                                id,
                                owner,
                                Some(revision),
                                "cleanup_pending",
                                None,
                            )
                            .await?,
                    )
                } else {
                    None
                };
                if artifact.kind == "project_stage" {
                    let root = ensure_stage_root(stage_root(self)).await?;
                    let stage_id = registered_stage(&artifact, &root)?;
                    let kind = artifact
                        .identity
                        .kind
                        .clone()
                        .ok_or_else(|| AppError::invalid("stage 类型缺失"))?;
                    let expected = artifact.identity.ownership_digest.clone();
                    let project_root = artifact.identity.canonical_path.clone();
                    if kind.ends_with("_preparing") {
                        if method != "transfer.artifact.cleanup" {
                            return Err(AppError::invalid("未完成 stage 只能查看或清理"));
                        }
                        let identity = artifact.identity.clone();
                        let check_root = root.clone();
                        let confirm_identity: Option<String> =
                            optional(&params, "confirm_stage_dir_identity")?;
                        let confirm_journal: Option<String> =
                            optional(&params, "confirm_journal_sha256")?;
                        blocking_leased(work_lease.clone(), move || {
                            let dir = check_root.join(stage_id.to_string());
                            match std::fs::symlink_metadata(&dir) {
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    return Ok(());
                                }
                                Err(error) => return Err(error.into()),
                                Ok(_) if identity.stage_dir_identity.is_none() => {
                                    let actual =
                                        binding_root_identity(dir.to_str().ok_or_else(|| {
                                            AppError::invalid("stage 路径不是 UTF-8")
                                        })?)?;
                                    if confirm_identity.as_deref() != Some(actual.as_str()) {
                                        return Err(AppError::new(
                                            "confirmation_required",
                                            "请核对并确认 stage 目录身份",
                                        ));
                                    }
                                }
                                Ok(_) => {}
                            }
                            if kind == "save_preparing" && dir.join("stage.json").exists() {
                                let stage = project::read_stage(&check_root, stage_id)?;
                                if identity.ownership_digest.as_deref()
                                    != Some(stage.preview.digest.as_str())
                                    || identity.canonical_path.as_deref()
                                        != Some(Path::new(&stage.identity.root))
                                {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "保存 stage 意图不匹配",
                                    ));
                                }
                                project::cleanup_stage(&check_root, stage_id)
                            } else if kind == "import_preparing" && dir.join("import.json").exists()
                            {
                                let journal = dir.join("import.json");
                                let meta = std::fs::symlink_metadata(&journal)?;
                                if meta.file_type().is_symlink()
                                    || !meta.is_file()
                                    || meta.len() > MIB as u64
                                {
                                    return Err(AppError::new("recovery_conflict", "导入日志无效"));
                                }
                                let bytes = std::fs::read(&journal)?;
                                let observed = digest_bytes(&bytes);
                                if identity.sha256.is_none() {
                                    if confirm_journal.as_deref() != Some(observed.as_str()) {
                                        return Err(AppError::new(
                                            "confirmation_required",
                                            "请核对并确认尚未登记的导入日志",
                                        ));
                                    }
                                    let entries: Vec<_> =
                                        std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
                                    if entries.len() != 1 || entries[0].file_name() != "import.json"
                                    {
                                        return Err(AppError::new(
                                            "recovery_conflict",
                                            "stage 包含未知内容",
                                        ));
                                    }
                                    project::read_import_journal(&check_root, stage_id)?;
                                    std::fs::remove_file(journal)?;
                                    std::fs::remove_dir(dir)?;
                                    return Ok(());
                                }
                                if identity.sha256.as_deref() != Some(observed.as_str()) {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "导入日志摘要已变化",
                                    ));
                                }
                                let stage = project::read_import_journal(&check_root, stage_id)?;
                                cleanup_import(&stage)
                            } else {
                                // 只删除已绑定身份的空目录；未知文件导致 remove_dir 失败并保留。
                                std::fs::remove_dir(dir)?;
                                Ok(())
                            }
                        })
                        .await?;
                    } else if kind == "save" {
                        let root_for_check = root.clone();
                        let stage = blocking_leased(work_lease.clone(), move || {
                            project::read_stage(&root_for_check, stage_id)
                        })
                        .await?;
                        if expected.as_deref() != Some(stage.preview.digest.as_str())
                            || project_root.as_deref() != Some(Path::new(&stage.identity.root))
                        {
                            return Err(AppError::new(
                                "recovery_conflict",
                                "stage 身份与登记不一致",
                            ));
                        }
                        if method == "transfer.artifact.export" {
                            let index: usize = model::param(&params, "index")?;
                            let side: String = model::param(&params, "side")?;
                            let change = stage
                                .preview
                                .changes
                                .get(index)
                                .ok_or_else(|| AppError::invalid("文件索引无效"))?;
                            let digest = match side.as_str() {
                                "old" => &change.old_digest,
                                "new" => &change.new_digest,
                                _ => return Err(AppError::invalid("side 必须为 old 或 new")),
                            }
                            .clone()
                            .ok_or_else(|| AppError::invalid("指定版本不存在"))?;
                            let input = Path::new(&stage.identity.stage_dir)
                                .join(format!("{index}.{side}"));
                            let output = export_path.unwrap();
                            blocking_leased(work_lease.clone(), move || {
                                let meta = std::fs::symlink_metadata(&input)?;
                                if meta.file_type().is_symlink()
                                    || !meta.is_file()
                                    || meta.len() > 16 * 1024 * 1024
                                {
                                    return Err(AppError::new("recovery_conflict", "恢复副本无效"));
                                }
                                let mut bytes = Vec::new();
                                std::fs::File::open(input)?
                                    .take(16 * 1024 * 1024 + 1)
                                    .read_to_end(&mut bytes)?;
                                if bytes.len() > 16 * 1024 * 1024 || digest_bytes(&bytes) != digest
                                {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "恢复副本已变化",
                                    ));
                                }
                                write_new(&output, &bytes)
                            })
                            .await?;
                        } else {
                            blocking_leased(work_lease.clone(), move || {
                                project::cleanup_stage(&root, stage_id)
                            })
                            .await?;
                        }
                    } else if kind == "import" {
                        let root_for_check = root.clone();
                        let stage = blocking_leased(work_lease.clone(), move || {
                            project::read_import_stage(&root_for_check, stage_id)
                        })
                        .await?;
                        if expected.as_ref() != stage.digests.get("datapier.project.json")
                            || project_root.as_deref() != Some(Path::new(&stage.destination))
                        {
                            return Err(AppError::new(
                                "recovery_conflict",
                                "导入 stage 身份与登记不一致",
                            ));
                        }
                        if method == "transfer.artifact.export" {
                            let output = export_path.unwrap();
                            blocking_leased(work_lease.clone(), move || {
                                let snapshot =
                                    project::open(&Path::new(&stage.stage_dir).join("project"))?;
                                project::export_archive(&snapshot, &output)
                            })
                            .await?;
                        } else {
                            blocking_leased(work_lease.clone(), move || cleanup_import(&stage))
                                .await?;
                        }
                    } else {
                        return Err(AppError::invalid("未知项目 stage 类型"));
                    }
                } else {
                    let identity = artifact.identity.clone();
                    if method == "transfer.artifact.export" {
                        let output = export_path.unwrap();
                        blocking_leased(work_lease.clone(), move || {
                            let input = verified_file(&identity)?;
                            let mut source = std::fs::File::open(&input)?;
                            let mut dest = std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(output)?;
                            std::io::copy(&mut source, &mut dest)?;
                            dest.sync_all()?;
                            verified_file(&identity)?;
                            Ok(())
                        })
                        .await?;
                    } else {
                        blocking_leased(work_lease.clone(), move || {
                            let path = verified_file(&identity)?;
                            std::fs::remove_file(path)?;
                            Ok(())
                        })
                        .await?;
                    }
                }
                if method == "transfer.artifact.cleanup" {
                    self.store
                        .transfer_update_artifact(id, owner, cleanup_revision, "cleaned", None)
                        .await?;
                    Ok(json!({"state":"cleaned"}))
                } else {
                    Ok(json!({"state":"published"}))
                }
            }
            _ => Err(AppError::new("method_not_found", "未知项目方法")),
        }
    }
}

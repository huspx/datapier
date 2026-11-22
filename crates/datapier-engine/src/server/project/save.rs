use super::super::transfer;
use super::*;

impl Engine {
    pub(super) async fn project_save_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        ws: String,
    ) -> Result<Value> {
        match method {
            "project.save.upload.begin" => {
                let project_id: Uuid = model::param(&params, "project_id")?;
                let slot = self.project_slot(owner, &ws, project_id)?;
                let baseline = slot.lock().await.digests.clone();
                let lease = self.reserve_project(owner, WORK_BYTES)?;
                let upload_id = Uuid::new_v4();
                let mut state = self.state.lock().unwrap();
                if !state.clients.contains(&owner)
                    || !state
                        .projects
                        .projects
                        .get(&(owner, ws.clone(), project_id))
                        .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    return Err(AppError::new("disconnected", "项目已关闭"));
                }
                state.projects.uploads.insert(
                    upload_id,
                    Upload {
                        owner,
                        workspace: ws,
                        project_id,
                        baseline,
                        revision: 1,
                        created: Instant::now(),
                        files: BTreeMap::new(),
                        bytes: 0,
                        _lease: lease,
                    },
                );
                Ok(json!({"upload_id":upload_id,"revision":1}))
            }
            "project.save.upload.chunk" | "project.save.upload.cancel" => {
                let upload_id: Uuid = model::param(&params, "upload_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let mut state = self.state.lock().unwrap();
                state.projects.prune();
                let upload = state
                    .projects
                    .uploads
                    .get_mut(&upload_id)
                    .ok_or_else(|| AppError::new("not_found", "上传不存在或已过期"))?;
                if upload.owner != owner || upload.workspace != ws || upload.revision != revision {
                    return Err(AppError::new("conflict", "上传所有者、工作区或版本不匹配"));
                }
                if method == "project.save.upload.cancel" {
                    state.projects.uploads.remove(&upload_id);
                    return Ok(json!({"cancelled":true}));
                }
                let path: String = model::param(&params, "path")?;
                let offset: usize = model::param(&params, "offset")?;
                let text: String = model::param(&params, "text")?;
                if path.is_empty()
                    || path.len() > 4096
                    || text.len() > 256 * 1024
                    || upload.bytes.saturating_add(text.len()) > 64 * MIB
                {
                    return Err(AppError::new("resource_limit", "上传分段或项目大小超限"));
                }
                if !upload.files.contains_key(&path) && (upload.files.len() >= 9_999 || offset != 0)
                {
                    return Err(AppError::new("resource_limit", "上传文件数或偏移无效"));
                }
                let file = upload.files.entry(path).or_default();
                if file.len() != offset || file.len().saturating_add(text.len()) > 16 * MIB {
                    return Err(AppError::new("conflict", "上传偏移或文件大小无效"));
                }
                file.push_str(&text);
                upload.bytes += text.len();
                upload.revision = upload
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| AppError::invalid("上传版本溢出"))?;
                upload.created = Instant::now();
                Ok(json!({"revision":upload.revision,"offset":file.len()}))
            }
            "project.save.prepare" => {
                let id: Uuid = model::param(&params, "project_id")?;
                if self
                    .project_slot(owner, &ws, id)?
                    .lock()
                    .await
                    .restored_baseline
                {
                    return Err(AppError::new("project_conflict", "请先核对并接受外部项目"));
                }
                let manifest: ProjectManifest = model::param(&params, "manifest")?;
                let upload_id: Option<Uuid> = optional(&params, "upload_id")?;
                let (patch, upload_lease): (BTreeMap<String, String>, Option<Arc<Lease>>) =
                    if let Some(upload_id) = upload_id {
                        let revision: u64 = model::param(&params, "revision")?;
                        let slot = self.project_slot(owner, &ws, id)?;
                        let snapshot = slot.lock().await;
                        let mut state = self.state.lock().unwrap();
                        state.projects.prune();
                        let upload = state
                            .projects
                            .uploads
                            .get(&upload_id)
                            .ok_or_else(|| AppError::new("not_found", "上传不存在或已过期"))?;
                        if upload.owner != owner
                            || upload.workspace != ws
                            || upload.project_id != id
                            || upload.revision != revision
                            || upload.baseline != snapshot.digests
                        {
                            return Err(AppError::new(
                                "conflict",
                                "上传所有者、版本或项目基线已变化",
                            ));
                        }
                        let Upload { files, _lease, .. } =
                            state.projects.uploads.remove(&upload_id).unwrap();
                        (files, Some(_lease))
                    } else {
                        (model::param(&params, "files")?, None)
                    };
                let replace: Option<Uuid> = optional(&params, "replace_plan_id")?;
                if let Some(previous) = replace {
                    let retired = {
                        let mut state = self.state.lock().unwrap();
                        let entry =
                            state.projects.plans.get(&previous).ok_or_else(|| {
                                AppError::new("not_found", "待替换的保存预览不存在")
                            })?;
                        if entry.owner != owner
                            || entry.workspace != ws
                            || entry.project_id != id
                            || Arc::strong_count(&entry.slot) != 1
                        {
                            return Err(AppError::new("busy", "保存预览不属于此项目或正在使用"));
                        }
                        state.projects.plans.remove(&previous)
                    };
                    blocking(move || {
                        drop(retired);
                        Ok(())
                    })
                    .await?;
                }
                let lease = self.reserve_project(owner, PLAN_BYTES)?;
                drop(upload_lease);
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await.snapshot.clone();
                let included: BTreeSet<_> = manifest
                    .queries
                    .iter()
                    .map(|q| q.path.clone())
                    .chain(manifest.transfers.iter().map(|t| t.path.clone()))
                    .collect();
                if patch.keys().any(|path| !included.contains(path)) {
                    return Err(AppError::invalid("保存内容包含未被新清单引用的文件"));
                }
                let mut files = snapshot.files.clone();
                files.extend(patch);
                files.retain(|path, _| included.contains(path));
                // 同时预留预览和未来快照，避免文件已发布后才发现没有读回额度。
                let future_bytes =
                    datapier_core::execution::migration::encoded_size(&(&manifest, &files))?
                        .checked_add(files.keys().map(|path| path.len() + 96).sum::<usize>())
                        .and_then(|bytes| bytes.checked_add(snapshot.root.len() + 8192))
                        .ok_or_else(|| AppError::new("resource_limit", "项目快照额度溢出"))?;
                let worker_lease = lease.clone();
                let preview = blocking(move || {
                    let _lease = worker_lease;
                    project::preview(&snapshot, manifest, files)
                })
                .await?;
                let retained = datapier_core::execution::migration::encoded_size(&preview)?
                    .checked_add(future_bytes)
                    .ok_or_else(|| AppError::new("resource_limit", "项目预览额度溢出"))?;
                let memory = Arc::new(
                    transfer::MemoryReservation::reserve(&self.store, owner, retained).await?,
                );
                let plan_id = Uuid::new_v4();
                let response = json!({"plan_id":plan_id,"revision":1,"digest":preview.digest,"changes":change_summaries(&preview, true)});
                let mut state = self.state.lock().unwrap();
                if !state.clients.contains(&owner)
                    || !state
                        .projects
                        .projects
                        .get(&(owner, ws.clone(), id))
                        .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    return Err(AppError::new("disconnected", "项目已关闭或客户端已断开"));
                }
                let plan = SavePlan {
                    owner,
                    workspace: ws.clone(),
                    snapshot: slot,
                    preview,
                    used: false,
                    _lease: lease,
                    memory,
                };
                state.projects.plans.insert(
                    plan_id,
                    PlanEntry {
                        owner,
                        workspace: ws,
                        project_id: id,
                        created: Instant::now(),
                        slot: Arc::new(AsyncMutex::new(plan)),
                    },
                );
                Ok(response)
            }
            "project.save.commit" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let digest: String = model::param(&params, "digest")?;
                let plan_slot = {
                    let mut state = self.state.lock().unwrap();
                    state.projects.prune();
                    state
                        .projects
                        .plans
                        .get(&id)
                        .map(|entry| entry.slot.clone())
                        .ok_or_else(|| AppError::new("not_found", "保存计划不存在或已过期"))?
                };
                let mut plan = plan_slot.lock().await;
                if plan.owner != owner
                    || plan.workspace != ws
                    || plan.used
                    || revision != 1
                    || digest != plan.preview.digest
                {
                    return Err(AppError::new(
                        "conflict",
                        "保存计划所有者、版本或摘要不匹配",
                    ));
                }
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.clients.contains(&owner)
                        || !state
                            .projects
                            .plans
                            .get(&id)
                            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &plan_slot))
                    {
                        return Err(AppError::new("conflict", "保存计划已失效"));
                    }
                    state.projects.plans.remove(&id);
                }
                plan.used = true;
                let snapshot_slot = plan.snapshot.clone();
                let mut snapshot = snapshot_slot.lock().await;
                if snapshot.manifest.id != plan.preview.project_id
                    || snapshot.root != plan.preview.root
                    || snapshot.digests != plan.preview.baseline
                {
                    return Err(AppError::new("conflict", "项目快照已变化"));
                }
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let (stage_id, preparing) = self
                    .begin_project_stage(owner, &root, &snapshot.root, "save", Some(&digest))
                    .await?;
                let preview = plan.preview.clone();
                let expected = digest.clone();
                let stage_root = root.clone();
                let worker_lease = plan._lease.clone();
                let stage = blocking(move || {
                    let _lease = worker_lease;
                    project::prepare_save_in(&preview, &expected, &stage_root, stage_id, true)
                })
                .await?;
                let mut ready_identity = preparing.identity.clone();
                ready_identity.kind = Some("save".into());
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
                let gate = self.artifact_gate(artifact.id);
                let _guard = gate.lock().await;
                owned(
                    &self.store.transfer_artifact(artifact.id).await?,
                    owner,
                    artifact.revision,
                )?;
                let stage_id = stage.identity.id;
                let result = blocking_leased(plan._lease.clone(), move || {
                    project::commit(&root, stage_id)
                })
                .await;
                match result {
                    Ok(_) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        let path = PathBuf::from(&snapshot.root);
                        let refreshed =
                            blocking_leased(plan._lease.clone(), move || project::open(&path))
                                .await?;
                        plan.memory
                            .resize(
                                owner,
                                datapier_core::execution::migration::encoded_size(&refreshed)?,
                            )
                            .await?;
                        snapshot.snapshot = Arc::new(ChargedSnapshot {
                            snapshot: refreshed,
                            _memory: plan.memory.clone(),
                        });
                        Ok(
                            json!({"project_id":snapshot.manifest.id,"state":"published","manifest":snapshot.manifest,"digests":snapshot.digests,"artifact_id":artifact.id}),
                        )
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
            _ => Err(AppError::new("method_not_found", "未知项目方法")),
        }
    }
}

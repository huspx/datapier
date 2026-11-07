use super::*;

fn auth_request_invalid(error: &AppError) -> bool {
    matches!(
        error.code.as_str(),
        "not_found" | "permission_denied" | "disconnected"
    )
}

fn auth_profile_matches(profile: &Profile, request: &AuthenticationRequest) -> bool {
    profile.revision == request.profile_revision
        && profile.archery().is_some_and(|connection| {
            connection.server_id == request.server_id
                && connection.instance_id == request.instance_id
        })
}

impl Engine {
    pub(super) fn mcp_auth_request_id(&self, owner: Uuid, connection_id: Uuid) -> Option<Uuid> {
        self.state
            .lock()
            .unwrap()
            .mcp
            .authentication
            .get(&(owner, connection_id))
            .map(|request| request.id)
    }

    fn mcp_remove_auth_id(&self, owner: Uuid, connection_id: Uuid, id: Option<Uuid>) {
        let mut state = self.state.lock().unwrap();
        let key = (owner, connection_id);
        if state
            .mcp
            .authentication
            .get(&key)
            .is_some_and(|current| Some(current.id) == id)
        {
            state.mcp.authentication.remove(&key);
        }
    }

    fn mcp_remove_auth_request(&self, request: &AuthenticationRequest) {
        self.mcp_remove_auth_id(request.owner, request.connection_id, Some(request.id));
    }

    pub(super) async fn mcp_record_auth(
        &self,
        owner: Uuid,
        profile: &Profile,
        previous: Option<Uuid>,
        auth_state: &str,
    ) -> Result<()> {
        let key = (owner, profile.id);
        if auth_state == "ready" {
            self.mcp_remove_auth_id(owner, profile.id, previous);
            return Ok(());
        }
        let Some(connection) = profile.archery() else {
            return Ok(());
        };
        if self
            .mcp_archery_status(owner, connection.server_id)
            .await?
            .0
            == "ready"
        {
            self.mcp_remove_auth_id(owner, profile.id, previous);
            return Ok(());
        }
        let latest = match self.mcp_profile(owner, profile.id).await {
            Ok(profile) => profile,
            Err(error) if auth_request_invalid(&error) => {
                self.mcp_remove_auth_id(owner, profile.id, previous);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if latest.revision != profile.revision
            || latest.archery().is_none_or(|current| {
                current.server_id != connection.server_id
                    || current.instance_id != connection.instance_id
            })
        {
            self.mcp_remove_auth_id(owner, profile.id, previous);
            return Ok(());
        }
        let mut state = self.state.lock().unwrap();
        if !state.clients.contains(&owner) || !state.contexts.contains_key(&owner) {
            return Ok(());
        }
        let current = state.mcp.authentication.get(&key);
        if current.map(|request| request.id) != previous {
            return Ok(());
        }
        if current.is_some_and(|request| auth_profile_matches(profile, request)) {
            return Ok(());
        }
        state.mcp.authentication.insert(
            key,
            AuthenticationRequest {
                id: Uuid::new_v4(),
                owner,
                connection_id: profile.id,
                server_id: connection.server_id,
                instance_id: connection.instance_id,
                profile_revision: profile.revision,
                controller: None,
            },
        );
        Ok(())
    }

    pub(super) async fn mcp_authentication_views(&self, viewer: Uuid) -> Result<Vec<Value>> {
        let requests: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .mcp
            .authentication
            .values()
            .cloned()
            .collect();
        let mut views = Vec::new();
        for request in requests {
            match self.store.archery_server(request.server_id).await {
                Ok(_) => (),
                Err(error) if auth_request_invalid(&error) => {
                    self.mcp_remove_auth_request(&request);
                    continue;
                }
                Err(error) => return Err(error),
            }
            let (auth_state, _) = self
                .mcp_archery_status(request.owner, request.server_id)
                .await?;
            if auth_state == "ready" {
                self.mcp_remove_auth_request(&request);
                continue;
            }
            // 状态查询存在 await；用随后读取的配置快照完成最终授权与身份校验。
            let profile = match self.mcp_profile(request.owner, request.connection_id).await {
                Ok(profile) if auth_profile_matches(&profile, &request) => profile,
                Ok(_) => {
                    self.mcp_remove_auth_request(&request);
                    continue;
                }
                Err(error) if auth_request_invalid(&error) => {
                    self.mcp_remove_auth_request(&request);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let controller = {
                let mut state = self.state.lock().unwrap();
                let online = Self::mcp_online_state(&state);
                let preferred =
                    Self::mcp_preferred_window_state(&state, &online, request.connection_id);
                let Some(current) = state
                    .mcp
                    .authentication
                    .get_mut(&(request.owner, request.connection_id))
                    .filter(|current| current.id == request.id)
                else {
                    continue;
                };
                if !current
                    .controller
                    .is_some_and(|controller| online.contains(&controller))
                {
                    current.controller = preferred;
                }
                current.controller
            };
            views.push(json!({"request_id":request.id,"connection_id":request.connection_id,"server_id":request.server_id,"connection_name":profile.name,"state":auth_state,"can_authenticate":controller == Some(viewer)}));
        }
        Ok(views)
    }
}

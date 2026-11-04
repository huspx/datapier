use crate::{Result, execution::ExecutionSink, model::*};
use reqwest::{
    Client, Url,
    cookie::{CookieStore, Jar},
    header,
};
use serde::Serialize;
use serde_json::Value as Json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "../../../tests/unit/archery/dispatch.rs"]
mod dispatch_tests;

#[cfg(test)]
#[path = "../../../tests/unit/archery/client.rs"]
mod client_tests;

const MAX_BODY: usize = 16 * 1024 * 1024;
// 为 IPC 响应信封保留空间，预算包含包裹字段。
const IPC_BUDGET: usize = 4 * 1024 * 1024 - 4096;

/// 统一服务 URL 身份；路径前缀属于服务身份的一部分。
pub fn normalize_server(server: &mut ArcheryServer) -> Result<()> {
    let mut url =
        Url::parse(&server.base_url).map_err(|_| AppError::invalid("无效 Archery 服务地址"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::invalid(
            "Archery 地址须为不含凭据、查询和片段的 HTTP(S) URL",
        ));
    }
    if server.username.trim().is_empty() {
        return Err(AppError::invalid("Archery 用户名不能为空"));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    server.base_url = url.to_string();
    Ok(())
}

/// 不含凭据的认证槽状态。
#[derive(Clone, Debug, Serialize)]
pub struct AuthStatus {
    pub state: String,
    pub generation: u64,
}

struct Transport {
    client: Client,
    jar: Arc<Jar>,
}
struct State {
    status: AuthStatus,
    transport: Option<Arc<Transport>>,
    challenge_deadline: Option<Instant>,
    http_tls: HttpTls,
    totp_mode: TotpMode,
    revision: u64,
}
impl State {
    fn expire_challenge(&mut self) {
        if self.status.state == "challenge_required"
            && self
                .challenge_deadline
                .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.status.state = "auth_required".into();
            self.transport = None;
            self.challenge_deadline = None;
        }
    }
}

/// 单个客户端与服务的认证上下文；认证转换独占，数据请求最多四个。
pub struct AuthSlot {
    server: ArcheryServer,
    base: Url,
    state: Mutex<State>,
    gate: RwLock<()>,
    permits: Semaphore,
    busy: AtomicUsize,
}
struct Busy<'a>(&'a AtomicUsize);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
impl AuthSlot {
    pub fn new(mut server: ArcheryServer) -> Result<Self> {
        normalize_server(&mut server)?;
        let base = Url::parse(&server.base_url).map_err(|_| AppError::invalid("无效服务地址"))?;
        let revision = server.revision;
        let http_tls = server.http_tls.clone();
        let totp_mode = server.totp_mode;
        Ok(Self {
            server,
            base,
            state: Mutex::new(State {
                status: AuthStatus {
                    state: "disconnected".into(),
                    generation: 0,
                },
                transport: None,
                challenge_deadline: None,
                revision,
                http_tls,
                totp_mode,
            }),
            gate: RwLock::new(()),
            permits: Semaphore::new(4),
            busy: AtomicUsize::new(0),
        })
    }
    fn activity(&self) -> Busy<'_> {
        self.busy.fetch_add(1, Ordering::AcqRel);
        Busy(&self.busy)
    }
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire) != 0
    }
    /// 被动读取状态，并清除已经过期的认证挑战。
    pub async fn status(&self) -> AuthStatus {
        let mut state = self.state.lock().await;
        state.expire_challenge();
        state.status.clone()
    }
    pub async fn rebind_revision(&self, revision: u64) -> Result<()> {
        self.state.lock().await.revision = revision;
        Ok(())
    }
    /// 将已验证的私有候选认证发布到稳定的共享认证槽。
    pub async fn publish_candidate(
        &self,
        candidate: &AuthSlot,
        server: &ArcheryServer,
    ) -> Result<()> {
        if std::ptr::eq(self, candidate) {
            return Err(AppError::invalid("认证候选必须使用独立上下文"));
        }
        let mut server = server.clone();
        normalize_server(&mut server)?;
        if self.server.id != server.id
            || self.server.base_url != server.base_url
            || self.server.username != server.username
            || candidate.server.id != server.id
            || candidate.server.base_url != server.base_url
            || candidate.server.username != server.username
        {
            return Err(AppError::conflict("认证候选的服务身份不一致"));
        }
        let (transport, candidate_revision, http_tls, totp_mode) = {
            let state = candidate.state.lock().await;
            if state.status.state != "ready" {
                return Err(AppError::new("auth_required", "认证候选尚未就绪"));
            }
            let transport = state
                .transport
                .clone()
                .ok_or_else(|| AppError::new("auth_required", "认证候选已失效"))?;
            (
                transport,
                state.revision,
                state.http_tls.clone(),
                state.totp_mode,
            )
        };
        if http_tls != server.http_tls
            || totp_mode != server.totp_mode
            || candidate_revision.checked_add(1) != Some(server.revision)
        {
            return Err(AppError::conflict("认证候选配置或修订不一致"));
        }
        let _gate = self.gate.write().await;
        let mut state = self.state.lock().await;
        let fresh_saved_slot = state.revision == server.revision
            && state.status.state == "disconnected"
            && state.status.generation == 0;
        if state.status.state == "closed"
            || (state.revision != candidate_revision && !fresh_saved_slot)
        {
            return Err(AppError::conflict("共享认证上下文已变化"));
        }
        let generation = state
            .status
            .generation
            .checked_add(1)
            .ok_or_else(|| AppError::new("internal", "认证代次已耗尽"))?;
        state.transport = Some(transport);
        state.http_tls = http_tls;
        state.totp_mode = totp_mode;
        state.revision = server.revision;
        state.challenge_deadline = None;
        state.status.generation = generation;
        state.status.state = "ready".into();
        Ok(())
    }
    pub(super) fn server_id(&self) -> uuid::Uuid {
        self.server.id
    }
    /// 结束未完成的认证轮次，不影响已就绪的共享会话。
    pub async fn cancel_authentication(&self) {
        let _guard = self.gate.write().await;
        let mut state = self.state.lock().await;
        if state.status.state != "ready" && state.status.state != "closed" {
            state.transport = None;
            state.challenge_deadline = None;
            state.status.state = "disconnected".into();
        }
    }
    pub async fn close(&self) {
        let _guard = self.gate.write().await;
        let mut s = self.state.lock().await;
        s.transport = None;
        s.challenge_deadline = None;
        s.status.state = "closed".into();
    }
    fn transport(&self, http_tls: &HttpTls) -> Result<Arc<Transport>> {
        let jar = Arc::new(Jar::default());
        let mut builder = Client::builder()
            .cookie_provider(jar.clone())
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10));
        if let Some(path) = &http_tls.ca_path {
            let pem = std::fs::read(path).map_err(|e| AppError::new("tls", e.to_string()))?;
            let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| AppError::new("tls", e.to_string()))?;
            if certificates.is_empty() {
                return Err(AppError::new("tls", "CA 证书为空"));
            }
            builder = builder.tls_certs_merge(certificates);
        }
        let client = builder
            .build()
            .map_err(|e| AppError::new("http", e.to_string()))?;
        Ok(Arc::new(Transport { client, jar }))
    }
    fn endpoint(&self, path: &str) -> Result<Url> {
        if path.starts_with('/') || path.contains("..") {
            return Err(AppError::invalid("无效 Archery 端点"));
        }
        self.base
            .join(path)
            .map_err(|_| AppError::invalid("无效 Archery 端点"))
    }
    fn csrf(&self, t: &Transport, url: &Url) -> Result<String> {
        let cookies = t
            .jar
            .cookies(url)
            .ok_or_else(|| AppError::new("remote_protocol", "缺少 CSRF Cookie"))?;
        cookies
            .to_str()
            .ok()
            .and_then(|s| {
                s.split(';')
                    .map(str::trim)
                    .find_map(|s| s.strip_prefix("csrftoken="))
            })
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| AppError::new("remote_protocol", "缺少 CSRF Cookie"))
    }
    async fn bytes(
        &self,
        mut response: reqwest::Response,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(AppError::new("remote_protocol", "不支持的 HTTP 内容编码"));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err(AppError::new(
                "response_too_large",
                "平台响应超过 16 MiB，请缩小查询范围",
            ));
        }
        let mut bytes = Vec::new();
        loop {
            let next = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), x = response.chunk() => x.map_err(|_| AppError::new("http", "读取平台响应失败"))? };
            let Some(chunk) = next else { break };
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(AppError::new(
                    "response_too_large",
                    "平台响应超过 16 MiB，请缩小查询范围",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        tokio::select! { biased; _ = cancel.cancelled() => Err(AppError::cancelled()), r = request.send() => r.map_err(|e| AppError::new("http", if e.is_timeout() { "平台请求超时" } else { "平台请求失败" })) }
    }
    fn http_status(&self, status: reqwest::StatusCode) -> Result<()> {
        let error = match status {
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                AppError::new("rate_limited", "平台限制请求频率")
            }
            reqwest::StatusCode::FORBIDDEN => AppError::new("permission_denied", "平台拒绝访问"),
            reqwest::StatusCode::UNAUTHORIZED => AppError::new("auth_required", "平台认证已失效"),
            _ if !status.is_success() => {
                AppError::new("http", format!("平台返回 HTTP {}", status.as_u16()))
            }
            _ => return Ok(()),
        };
        Err(error.with_details(serde_json::json!({"http_status":status.as_u16()})))
    }
    async fn login_page(
        &self,
        t: &Transport,
        path: &str,
        follow: bool,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let mut url = self.endpoint(path)?;
        for attempt in 0..=3 {
            let response = self
                .send(
                    t.client.get(url.clone()).timeout(Duration::from_secs(30)),
                    cancel,
                )
                .await?;
            if response.status().is_redirection() {
                if !follow || attempt == 3 {
                    return Err(AppError::new(
                        "interactive_login_required",
                        "登录页面重定向需要交互式登录",
                    ));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| AppError::new("remote_protocol", "重定向缺少目标"))?;
                let next = url
                    .join(location)
                    .map_err(|_| AppError::new("remote_protocol", "无效登录重定向"))?;
                if next.origin() != self.base.origin() || !next.path().starts_with(self.base.path())
                {
                    return Err(AppError::new(
                        "interactive_login_required",
                        "登录跳转超出服务地址",
                    ));
                }
                url = next;
                continue;
            }
            self.http_status(response.status())?;
            return String::from_utf8(self.bytes(response, cancel).await?)
                .map_err(|_| AppError::new("remote_protocol", "登录页面不是 UTF-8"));
        }
        Err(AppError::new("remote_protocol", "登录跳转过多"))
    }
    async fn post_raw(
        &self,
        t: &Transport,
        path: &str,
        form: &[(&str, String)],
        long: bool,
        sink: Option<&ExecutionSink>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let url = self.endpoint(path)?;
        let csrf = self.csrf(t, &url)?;
        let origin = self.base.origin().ascii_serialization();
        let request = t
            .client
            .post(url)
            .timeout(Duration::from_secs(if long { 120 } else { 30 }))
            .header("X-CSRFToken", csrf)
            .header(header::REFERER, self.base.as_str())
            .header(header::ORIGIN, origin)
            .form(form);
        if let Some(sink) = sink {
            sink.mark_sent(false).await?;
        }
        self.send(request, cancel).await
    }
    async fn decode<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        response: reqwest::Response,
        cancel: &CancellationToken,
    ) -> Result<T> {
        self.http_status(response.status())?;
        let bytes = self.bytes(response, cancel).await?;
        let parse = tokio::task::spawn_blocking(move || {
            serde_json::from_slice(&bytes)
                .map_err(|_| AppError::new("remote_protocol", "平台返回的 JSON 结构无效"))
        });
        tokio::select! { biased; _ = cancel.cancelled() => Err(AppError::cancelled()), result = parse => result.map_err(|_| AppError::new("internal", "平台响应解析失败"))? }
    }
    async fn auth_json(
        &self,
        t: &Transport,
        path: &str,
        form: &[(&str, String)],
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let response = self.post_raw(t, path, form, false, None, cancel).await?;
        if response.status().is_redirection() {
            return Err(AppError::new(
                "interactive_login_required",
                "认证请求被重定向",
            ));
        }
        self.decode(response, cancel).await
    }
    pub async fn authenticate(
        &self,
        secret: String,
        cancel: &CancellationToken,
    ) -> Result<AuthStatus> {
        let _activity = self.activity();
        let _gate = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), g = self.gate.write() => g };
        let http_tls = {
            let mut s = self.state.lock().await;
            s.status.state = "authenticating".into();
            s.transport = None;
            s.challenge_deadline = None;
            s.http_tls.clone()
        };
        let challenge_deadline = Instant::now() + Duration::from_secs(300);
        let result = async {
            let t = self.transport(&http_tls)?;
            self.login_page(&t, "login/", true, cancel).await?;
            let data = self
                .auth_json(
                    &t,
                    "authenticate/",
                    &[
                        ("username", self.server.username.clone()),
                        ("password", secret),
                    ],
                    cancel,
                )
                .await?;
            if data.get("status").and_then(Json::as_i64) != Some(0) {
                return Err(AppError::new("auth_failed", "用户名或密码认证失败"));
            }
            match data.get("data") {
                Some(Json::Null) => Ok((t, false)),
                Some(Json::String(key))
                    if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    t.jar.add_cookie_str(
                        &format!("sessionid={key}; Path={}; HttpOnly", self.base.path()),
                        &self.base,
                    );
                    let page = self.login_page(&t, "login/2fa/", false, cancel).await?;
                    // v1.8.4 由服务端选择 TOTP 分支并渲染专用表单；后续版本在 JS 中声明模式。
                    // 仅识别完整的旧登录表单特征，不能将绑定页面或其他验证方式视为 TOTP。
                    let modern_totp = page.contains("'verify_only' === 'verify_config'")
                        && (page.contains("'code': 'totp'") || page.contains("\"code\": \"totp\""));
                    let legacy_totp = page.contains("<form class=\"login-form fade-in-effect\" id=\"auth\" method=\"post\" role=\"form\">")
                        && page.contains("<label class=\"control-label\" for=\"otpCode\">OTP验证码</label>")
                        && page.contains("id=\"otpCode\" name=\"otpCode\"")
                        && page.contains("id=\"btnAuth\"")
                        && page.contains("/api/v1/user/2fa/verify/")
                        && !page.contains("id=\"btnCaptcha\"")
                        && !page.contains("bind_config")
                        && !page.contains("name=\"key\"")
                        && !page.contains("key:");
                    if !modern_totp && !legacy_totp {
                        return Err(AppError::new(
                            "interactive_login_required",
                            "请先在平台完成 TOTP 配置",
                        ));
                    }
                    Ok((t, true))
                }
                _ => Err(AppError::new("remote_protocol", "认证响应格式未知")),
            }
        }
        .await;
        let mut s = self.state.lock().await;
        match result {
            Ok((transport, challenge)) if !cancel.is_cancelled() => {
                s.transport = Some(transport);
                s.challenge_deadline = challenge.then_some(challenge_deadline);
                s.status.state = if challenge {
                    "challenge_required"
                } else {
                    s.status.generation += 1;
                    "ready"
                }
                .into();
                Ok(s.status.clone())
            }
            Ok(_) => {
                s.status.state = "disconnected".into();
                Err(AppError::cancelled())
            }
            Err(e) => {
                s.status.state = if e.code == "cancelled" {
                    "disconnected"
                } else {
                    "auth_failed"
                }
                .into();
                Err(e)
            }
        }
    }
    pub async fn verify(&self, otp: String, cancel: &CancellationToken) -> Result<AuthStatus> {
        let _activity = self.activity();
        let _gate = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), g = self.gate.write() => g };
        if otp.is_empty() || !otp.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AppError::invalid("OTP 格式无效"));
        }
        let t = {
            let mut s = self.state.lock().await;
            s.expire_challenge();
            if s.status.state != "challenge_required" || s.challenge_deadline.is_none() {
                return Err(AppError::new("auth_required", "没有有效的认证挑战"));
            }
            s.transport
                .clone()
                .ok_or_else(|| AppError::new("auth_required", "认证挑战已失效"))?
        };
        let response = self
            .auth_json(
                &t,
                "api/v1/user/2fa/verify/",
                &[
                    ("engineer", self.server.username.clone()),
                    ("auth_type", "totp".into()),
                    ("otp", otp),
                ],
                cancel,
            )
            .await?;
        if response.get("status").and_then(Json::as_i64) != Some(0) {
            return Err(AppError::new("auth_failed", "OTP 验证失败"));
        }
        ensure_not_cancelled(cancel)?;
        let mut s = self.state.lock().await;
        // 状态观察可能在请求期间使挑战过期，不能重新发布已失效的会话。
        s.expire_challenge();
        if s.status.state != "challenge_required" || s.challenge_deadline.is_none() {
            return Err(AppError::new("auth_required", "没有有效的认证挑战"));
        }
        s.status.generation += 1;
        s.status.state = "ready".into();
        s.challenge_deadline = None;
        Ok(s.status.clone())
    }
    async fn request<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        args: &[(&str, String)],
        post: bool,
        long: bool,
        sink: Option<&ExecutionSink>,
        cancel: &CancellationToken,
    ) -> Result<T> {
        let _activity = self.activity();
        let _gate = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), g = self.gate.read() => g };
        let _permit = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), p = self.permits.acquire() => p.map_err(|_| AppError::new("internal", "请求额度已关闭"))? };
        let (transport, generation) = {
            let s = self.state.lock().await;
            if s.status.state != "ready" {
                return Err(AppError::new("auth_required", "请重新登录 Archery"));
            }
            (
                s.transport
                    .clone()
                    .ok_or_else(|| AppError::new("auth_required", "认证已失效"))?,
                s.status.generation,
            )
        };
        let response = if post {
            self.post_raw(&transport, path, args, long, sink, cancel)
                .await?
        } else {
            let url = self.endpoint(path)?;
            self.send(
                transport
                    .client
                    .get(url)
                    .query(args)
                    .timeout(Duration::from_secs(if long { 120 } else { 30 })),
                cancel,
            )
            .await?
        };
        let login_redirect = response.status().is_redirection()
            && response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|location| self.base.join(location).ok())
                .is_some_and(|url| {
                    url.origin() == self.base.origin()
                        && url.path() == format!("{}login/", self.base.path())
                });
        if response.status().is_redirection() && !login_redirect {
            return Err(AppError::new(
                "remote_protocol",
                "平台业务请求出现意外重定向",
            ));
        }
        if login_redirect || response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let mut s = self.state.lock().await;
            if s.status.generation == generation {
                s.status.state = "auth_required".into();
                s.transport = None;
            }
            return Err(AppError::new("auth_required", "平台认证已失效"));
        }
        self.decode(response, cancel).await
    }
    pub(super) async fn get(
        &self,
        path: &str,
        params: &[(&str, String)],
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.request(path, params, false, false, None, cancel).await
    }
    pub(super) async fn post_execution<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        form: &[(&str, String)],
        long: bool,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<T> {
        self.request(path, form, true, long, Some(sink), cancel)
            .await
    }
    pub async fn instances(&self, cancel: &CancellationToken) -> Result<Vec<ArcheryInstance>> {
        let response = self.get("group/user_all_instances/", &[], cancel).await?;
        let rows = web_data(&response)?
            .as_array()
            .ok_or_else(|| AppError::new("remote_protocol", "实例响应不是数组"))?;
        let mut result = Vec::new();
        for row in rows {
            let provider = match row.get("db_type").and_then(Json::as_str) {
                Some("mysql") => ProviderKind::Mysql,
                Some("redis") => ProviderKind::Redis,
                Some(_) => continue,
                None => return Err(AppError::new("remote_protocol", "实例缺少数据库类型")),
            };
            let id = row
                .get("id")
                .and_then(Json::as_i64)
                .ok_or_else(|| AppError::new("remote_protocol", "实例缺少 ID"))?;
            let name = row
                .get("instance_name")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::new("remote_protocol", "实例缺少名称"))?
                .to_owned();
            result.push(ArcheryInstance { id, name, provider });
        }
        budget(&result)?;
        Ok(result)
    }
    pub async fn databases(
        &self,
        instance: &ArcheryInstance,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>> {
        let instances = self.instances(cancel).await?;
        if !instances.iter().any(|x| {
            x.id == instance.id && x.name == instance.name && x.provider == instance.provider
        }) {
            return Err(AppError::new("target_changed", "平台实例绑定已变化"));
        }
        let response = self
            .get(
                "instance/instance_resource/",
                &[
                    ("instance_id", instance.id.to_string()),
                    ("resource_type", "database".into()),
                ],
                cancel,
            )
            .await?;
        let rows = web_data(&response)?
            .as_array()
            .ok_or_else(|| AppError::new("remote_protocol", "数据库响应不是数组"))?;
        let mut result = Vec::new();
        for row in rows {
            let name = row
                .as_str()
                .or_else(|| {
                    row.as_array()
                        .and_then(|r| if r.len() == 1 { r[0].as_str() } else { None })
                })
                .ok_or_else(|| AppError::new("remote_protocol", "数据库名称格式未知"))?;
            if instance.provider == ProviderKind::Redis
                && name
                    .parse::<u32>()
                    .ok()
                    .is_none_or(|v| v.to_string() != name)
            {
                return Err(AppError::new("remote_protocol", "Redis DB 格式无效"));
            }
            if !result.iter().any(|s| s == name) {
                result.push(name.to_owned());
            }
        }
        budget(&result)?;
        Ok(result)
    }
    pub async fn query_history(
        &self,
        offset: u64,
        limit: u64,
        search: &str,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        if limit == 0 || limit > 100 {
            return Err(AppError::invalid("历史条数须在 1 到 100 之间"));
        }
        let result = self
            .get(
                "query/querylog/",
                &[
                    ("offset", offset.to_string()),
                    ("limit", limit.to_string()),
                    ("search", search.to_owned()),
                ],
                cancel,
            )
            .await?;
        if result.get("total").and_then(Json::as_u64).is_none()
            || result.get("rows").and_then(Json::as_array).is_none()
        {
            return Err(AppError::new("remote_protocol", "平台历史响应格式未知"));
        }
        budget(&result)?;
        Ok(result)
    }
}
pub(super) fn web_data(response: &Json) -> Result<&Json> {
    match response.get("status").and_then(Json::as_i64) {
        Some(0) => response
            .get("data")
            .ok_or_else(|| AppError::new("remote_protocol", "平台响应缺少 data")),
        Some(_) => Err(AppError::new(
            "remote_error",
            response
                .get("msg")
                .and_then(Json::as_str)
                .unwrap_or("平台请求失败")
                .to_owned(),
        )),
        None => Err(AppError::new("remote_protocol", "平台响应缺少状态")),
    }
}
pub(super) fn budget<T: Serialize>(value: &T) -> Result<()> {
    if serde_json::to_vec(value)?.len() > IPC_BUDGET {
        Err(AppError::new(
            "response_too_large",
            "平台响应超过 IPC 预算，请缩小范围",
        ))
    } else {
        Ok(())
    }
}

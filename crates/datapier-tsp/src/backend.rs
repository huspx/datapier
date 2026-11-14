use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{ResultPage, Value as DataValue};
use datapier_core::{Result, model::AppError};
use datapier_engine::{Client, ClientOptions, EngineEvent};
use serde_json::Value;
use serde_json::json;
use std::{path::PathBuf, thread};
use tern_sdk::term::Waker;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

const REDIS_COPY_METHOD: &str = "local.redis.copy_value";
const REDIS_ROWS_METHOD: &str = "local.redis.selected_rows";
const GRID_COPY_METHOD: &str = "local.grid.copy";
const CLI_COPY_METHOD: &str = "local.cli.copy";

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct CliCopyRecord {
    pub instance: String,
    pub database: String,
    pub command: String,
    pub state: String,
    pub error: String,
    pub results: Vec<String>,
}

#[derive(serde::Deserialize)]
struct CliCopyRequest {
    records: Vec<CliCopyRecord>,
    empty: String,
}

// 复制的是执行日志及其全部缓存，不随当前页、旧记录选中态或字段预览截断。
async fn cli_copy_log(client: &Client, request: CliCopyRequest) -> Result<String> {
    use std::fmt::Write as _;
    let mut text = String::new();
    for record in request.records {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "{} · DB {} · {}\n› {}",
            record.instance, record.database, record.state, record.command
        );
        if !record.error.is_empty() {
            let _ = writeln!(text, "{}", record.error);
        }
        for id in record.results {
            let result_id =
                Uuid::parse_str(&id).map_err(|error| AppError::invalid(error.to_string()))?;
            let mut start = 0;
            let mut count = None;
            loop {
                let page: ResultPage = serde_json::from_value(client.request(
                    "result.read",
                    json!({"result_id":id,"start":start,"max_rows":200,"max_bytes":1048576}),
                ).await?)?;
                if page.result.id != result_id
                    || page.start != start
                    || count.is_some_and(|count| count != page.result.row_count)
                {
                    return Err(AppError::invalid("CLI 缓存结果已变化"));
                }
                if start == 0
                    && let Some(db) = page.result.summary.get("redis_db").and_then(Value::as_u64)
                {
                    let _ = writeln!(text, "DB {db}");
                }
                count = Some(page.result.row_count);
                let end = start + page.rows.len() as u64;
                if page.result.row_count == 0 {
                    let _ = writeln!(text, "{}", request.empty);
                } else {
                    let rows = redis_resolve_rows(client, page.rows, false).await?;
                    text.push_str(&crate::grid::encode_tsv(&rows)?);
                }
                match page.next {
                    Some(next) if next == end && next > start && next < page.result.row_count => {
                        start = next
                    }
                    None if end == page.result.row_count => break,
                    _ => return Err(AppError::invalid("CLI 缓存分页不完整")),
                }
            }
        }
    }
    Ok(text)
}
fn decode_large(data: &[u8], kind: &str) -> Result<DataValue> {
    let full: DataValue = serde_json::from_slice(data)?;
    let actual = match &full {
        DataValue::Null => "null",
        DataValue::Missing => "missing",
        DataValue::Bool(_) => "bool",
        DataValue::Integer(_) => "integer",
        DataValue::Unsigned(_) => "unsigned",
        DataValue::Decimal(_) => "decimal",
        DataValue::Float(_) => "float",
        DataValue::Text(_) => "text",
        DataValue::Bytes(_) => "bytes",
        DataValue::Date(_) => "date",
        DataValue::Time(_) => "time",
        DataValue::DateTime(_) => "date_time",
        DataValue::Json(_) => "json",
        DataValue::Array(_) => "array",
        DataValue::Map(_) => "map",
        DataValue::Large { .. } => return Err(AppError::invalid("大字段尚未完整读取")),
    };
    if actual != kind {
        return Err(AppError::invalid("大字段类型不匹配"));
    }
    Ok(full)
}

// 大字段分块保存的是完整类型化值，必须检查长度、连续性和原始类型。
async fn redis_resolve_rows(
    client: &Client,
    mut rows: Vec<Vec<DataValue>>,
    identity_only: bool,
) -> Result<Vec<Vec<DataValue>>> {
    for row in &mut rows {
        // 删除只需要第一列身份；复制必须解析所有列。
        for value in row
            .iter_mut()
            .take(if identity_only { 1 } else { usize::MAX })
        {
            let DataValue::Large {
                id,
                bytes,
                original_type,
                ..
            } = value
            else {
                continue;
            };
            let (id, expected, kind) = (*id, *bytes, original_type.clone());
            let mut offset = 0u64;
            let mut data = Vec::new();
            loop {
                let chunk = client
                    .request(
                        "result.value",
                        json!({"value_id":id,"offset":offset,"max_bytes":262144}),
                    )
                    .await?;
                if chunk.get("offset").and_then(Value::as_u64) != Some(offset)
                    || chunk.get("total_bytes").and_then(Value::as_u64) != Some(expected)
                {
                    return Err(AppError::invalid("Redis 大字段分块不一致"));
                }
                let encoded = chunk
                    .get("data_b64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("缺少 Redis 大字段分块"))?;
                let decoded = STANDARD
                    .decode(encoded)
                    .map_err(|e| AppError::invalid(e.to_string()))?;
                let end = offset
                    .checked_add(decoded.len() as u64)
                    .ok_or_else(|| AppError::invalid("Redis 大字段长度溢出"))?;
                data.extend_from_slice(&decoded);
                match chunk.get("next") {
                    Some(Value::Null) | None if end == expected => break,
                    Some(next) if next.as_u64() == Some(end) && end > offset && end < expected => {
                        offset = end
                    }
                    _ => return Err(AppError::invalid("Redis 大字段分块不连续")),
                }
            }
            *value = decode_large(&data, &kind)?;
        }
    }
    Ok(rows)
}

// 大字段是类型化 JSON 的分块，而不是原始 Redis 字节。
async fn redis_copy_value(client: &Client, result_id: Uuid, kind: &str) -> Result<String> {
    let mut start = 0u64;
    let mut row_count = None;
    let mut rows = Vec::new();
    loop {
        let page: ResultPage = serde_json::from_value(
            client
                .request(
                    "result.read",
                    json!({"result_id":result_id,"start":start,"max_rows":200,"max_bytes":1048576}),
                )
                .await?,
        )?;
        if page.result.id != result_id
            || page.result.kind != "redis"
            || !page.result.complete
            || page.start != start
            || row_count.is_some_and(|count| count != page.result.row_count)
        {
            return Err(AppError::invalid("Redis 缓存结果不完整或已变化"));
        }
        row_count = Some(page.result.row_count);
        let end = start
            .checked_add(page.rows.len() as u64)
            .ok_or_else(|| AppError::invalid("Redis 结果行数溢出"))?;
        for mut row in page.rows {
            for value in &mut row {
                if let DataValue::Large { id, bytes, .. } = value {
                    let id = *id;
                    let expected = *bytes;
                    let mut offset = 0u64;
                    let mut data = Vec::new();
                    loop {
                        let chunk = client
                            .request(
                                "result.value",
                                json!({"value_id":id,"offset":offset,"max_bytes":262144}),
                            )
                            .await?;
                        if chunk.get("offset").and_then(Value::as_u64) != Some(offset)
                            || chunk.get("total_bytes").and_then(Value::as_u64) != Some(expected)
                        {
                            return Err(AppError::invalid("Redis 大字段分块不一致"));
                        }
                        let encoded = chunk
                            .get("data_b64")
                            .and_then(Value::as_str)
                            .ok_or_else(|| AppError::invalid("缺少 Redis 大字段分块"))?;
                        let decoded = STANDARD
                            .decode(encoded)
                            .map_err(|e| AppError::invalid(e.to_string()))?;
                        let end = offset
                            .checked_add(decoded.len() as u64)
                            .ok_or_else(|| AppError::invalid("Redis 大字段长度溢出"))?;
                        data.extend_from_slice(&decoded);
                        match chunk.get("next") {
                            Some(Value::Null) | None if end == expected => break,
                            Some(next)
                                if next.as_u64() == Some(end) && end > offset && end < expected =>
                            {
                                offset = end
                            }
                            _ => return Err(AppError::invalid("Redis 大字段分块不连续")),
                        }
                    }
                    *value = serde_json::from_slice(&data)?;
                }
            }
            rows.push(row);
        }
        match page.next {
            Some(next) if next == end && next > start && next < page.result.row_count => {
                start = next
            }
            None if end == page.result.row_count => break,
            _ => return Err(AppError::invalid("Redis 缓存分页不完整")),
        }
    }
    redis_copy_encode(kind, rows)
}

// 结构类型统一用 Base64 对每个字节串编码，避免文本前缀和非 UTF-8 歧义。
fn redis_copy_encode(kind: &str, rows: Vec<Vec<DataValue>>) -> Result<String> {
    fn bytes(value: DataValue) -> Result<String> {
        match value {
            DataValue::Bytes(encoded) => {
                STANDARD
                    .decode(&encoded)
                    .map_err(|e| AppError::invalid(e.to_string()))?;
                Ok(encoded)
            }
            _ => Err(AppError::invalid("Redis 结果类型与资源类型不匹配")),
        }
    }
    if kind == "string" {
        let [row]: [Vec<DataValue>; 1] = rows
            .try_into()
            .map_err(|_| AppError::invalid("Redis String 结果不完整"))?;
        let [DataValue::Bytes(encoded)]: [DataValue; 1] = row
            .try_into()
            .map_err(|_| AppError::invalid("Redis String 结果不完整"))?
        else {
            return Err(AppError::invalid("Redis 结果类型与资源类型不匹配"));
        };
        let raw = STANDARD
            .decode(&encoded)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        return Ok(match String::from_utf8(raw) {
            Ok(text) => text,
            Err(_) => format!("base64:{encoded}"),
        });
    }
    if !matches!(kind, "hash" | "list" | "set" | "zset") {
        return Err(AppError::invalid("不支持的 Redis 类型"));
    }
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let len = row.len();
        let mut fields = row.into_iter();
        let mut entry = serde_json::Map::new();
        match (kind, len) {
            ("hash", 2) => {
                entry.insert(
                    "field_b64".into(),
                    Value::String(bytes(fields.next().unwrap())?),
                );
                entry.insert(
                    "value_b64".into(),
                    Value::String(bytes(fields.next().unwrap())?),
                );
            }
            ("list", 2) => {
                let Some(DataValue::Integer(index)) = fields.next() else {
                    return Err(AppError::invalid("Redis 结果类型与资源类型不匹配"));
                };
                entry.insert("index".into(), Value::String(index));
                entry.insert(
                    "value_b64".into(),
                    Value::String(bytes(fields.next().unwrap())?),
                );
            }
            ("set", 1) => {
                entry.insert(
                    "member_b64".into(),
                    Value::String(bytes(fields.next().unwrap())?),
                );
            }
            ("zset", 2) => {
                entry.insert(
                    "member_b64".into(),
                    Value::String(bytes(fields.next().unwrap())?),
                );
                let Some(DataValue::Float(score)) = fields.next() else {
                    return Err(AppError::invalid("Redis 结果类型与资源类型不匹配"));
                };
                entry.insert("score".into(), Value::String(score));
            }
            _ => return Err(AppError::invalid("Redis 结果类型与资源类型不匹配")),
        }
        entries.push(Value::Object(entry));
    }
    let mut root = serde_json::Map::new();
    root.insert("type".into(), Value::String(kind.into()));
    root.insert("entries".into(), Value::Array(entries));
    Ok(serde_json::to_string_pretty(&Value::Object(root))?)
}

// 选中条目以完整类型化值输出，整数精度和无穷浮点均可往返。
fn redis_copy_selected_encode(kind: &str, rows: Vec<Vec<DataValue>>) -> Result<String> {
    let fields: &[&str] = match kind {
        "hash" => &["field", "value"],
        "list" => &["index", "value"],
        "set" => &["member"],
        "zset" => &["member", "score"],
        _ => return Err(AppError::invalid("不支持的 Redis 类型")),
    };
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let valid = matches!(
            (kind, row.as_slice()),
            ("hash", [DataValue::Bytes(_), DataValue::Bytes(_)])
                | ("list", [DataValue::Integer(_), DataValue::Bytes(_)])
                | ("set", [DataValue::Bytes(_)])
                | ("zset", [DataValue::Bytes(_), DataValue::Float(_)])
        );
        if !valid {
            return Err(AppError::invalid("Redis 结果类型与资源类型不匹配"));
        }
        let mut entry = serde_json::Map::new();
        for (name, value) in fields.iter().zip(row) {
            if let DataValue::Bytes(encoded) = &value {
                STANDARD
                    .decode(encoded)
                    .map_err(|e| AppError::invalid(e.to_string()))?;
            }
            entry.insert((*name).into(), serde_json::to_value(value)?);
        }
        entries.push(Value::Object(entry));
    }
    Ok(serde_json::to_string_pretty(
        &json!({"type":kind,"entries":entries}),
    )?)
}

/// 发往独立 IPC 线程的请求；tag 用于将异步响应路由回发起方。
pub struct Request {
    pub tag: String,
    pub method: String,
    pub params: Value,
}
/// IPC 请求的结果或 Engine 事件，使用 tag 区分来源。
pub struct Response {
    pub tag: String,
    pub result: Result<Value>,
}

/// UI 线程与 Tokio IPC 线程之间的有界队列桥接，不持有 TSP Session。
/// 请求和响应都可能因队列关闭或容量限制失败；waker 只通知 UI 检查队列。
pub struct Backend {
    commands: mpsc::Sender<Request>,
    responses: mpsc::Receiver<Response>,
    thread: Option<thread::JoinHandle<()>>,
}

async fn consume(
    incoming: &mut mpsc::Receiver<Request>,
    events: &mut broadcast::Receiver<EngineEvent>,
    responses: &mpsc::Sender<Response>,
    mut dispatch: impl FnMut(Request),
    mut wake: impl FnMut(),
) {
    loop {
        tokio::select! {
            request = incoming.recv() => {
                let Some(request) = request else { break };
                dispatch(request);
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if let Ok(value) = serde_json::to_value(event) {
                        if responses.send(Response {
                            tag: "execution.event".into(),
                            result: Ok(value),
                        }).await.is_err() {
                            break;
                        }
                        wake();
                    }
                }
                // 事件可能被丢弃；任务状态以 execution.get 和周期轮询为准。
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

impl Backend {
    /// 启动后台 runtime 和 Engine 连接；线程创建失败时返回错误。
    /// 连接和 runtime 的异步失败通过响应队列报告。
    pub fn start(root: PathBuf, workspace: String, waker: Waker) -> Result<Self> {
        let (commands, mut incoming) = mpsc::channel::<Request>(64);
        let (responses, receiver) = mpsc::channel::<Response>(64);
        let worker = thread::Builder::new()
            .name("datapier-ipc".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = responses.blocking_send(Response {
                            tag: "connect".into(),
                            result: Err(AppError::new("runtime", error.to_string())),
                        });
                        waker.wake();
                        return;
                    }
                };
                runtime.block_on(async move {
                    let client = match Client::connect_with(
                        &root,
                        ClientOptions {
                            kind: datapier_core::model::ClientKind::Tsp,
                            workspace,
                            ..ClientOptions::default()
                        },
                    )
                    .await
                    {
                        Ok(client) => client,
                        Err(error) => {
                            let _ = responses
                                .send(Response {
                                    tag: "connect".into(),
                                    result: Err(error),
                                })
                                .await;
                            waker.wake();
                            return;
                        }
                    };
                    let mut events = client.subscribe_events();

                    if responses
                        .send(Response {
                            tag: "connect".into(),
                            result: Ok(Value::Null),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    waker.wake();
                    consume(
                        &mut incoming,
                        &mut events,
                        &responses,
                        |request| {
                            let client = client.clone();
                            let responses = responses.clone();
                            let waker = waker.clone();
                            tokio::spawn(async move {
                                let result = if request.method == REDIS_COPY_METHOD {
                                    match (
                                        request
                                            .params
                                            .get("result_id")
                                            .and_then(Value::as_str)
                                            .and_then(|id| Uuid::parse_str(id).ok()),
                                        request.params.get("kind").and_then(Value::as_str),
                                    ) {
                                        (Some(id), Some(kind)) => {
                                            redis_copy_value(&client, id, kind)
                                                .await
                                                .map(Value::String)
                                        }
                                        _ => Err(AppError::invalid("缺少 Redis 复制上下文")),
                                    }
                                } else if request.method == REDIS_ROWS_METHOD {
                                    let rows = serde_json::from_value::<Vec<Vec<DataValue>>>(
                                        request.params.get("rows").cloned().unwrap_or(Value::Null),
                                    );
                                    match rows {
                                        Ok(rows) => match redis_resolve_rows(
                                            &client,
                                            rows,
                                            request
                                                .params
                                                .get("kind")
                                                .and_then(Value::as_str)
                                                .is_none(),
                                        )
                                        .await
                                        {
                                            Ok(rows) => match request
                                                .params
                                                .get("kind")
                                                .and_then(Value::as_str)
                                            {
                                                Some(kind) => {
                                                    redis_copy_selected_encode(kind, rows)
                                                        .map(Value::String)
                                                }
                                                None => {
                                                    serde_json::to_value(rows).map_err(Into::into)
                                                }
                                            },
                                            Err(error) => Err(error),
                                        },
                                        Err(error) => Err(error.into()),
                                    }
                                } else if request.method == GRID_COPY_METHOD {
                                    match serde_json::from_value::<Vec<Vec<DataValue>>>(
                                        request.params.get("rows").cloned().unwrap_or(Value::Null),
                                    ) {
                                        Ok(rows) => {
                                            match redis_resolve_rows(&client, rows, false).await {
                                                Ok(rows) => crate::grid::encode_tsv(&rows)
                                                    .map(Value::String),
                                                Err(error) => Err(error),
                                            }
                                        }
                                        Err(error) => Err(error.into()),
                                    }
                                } else if request.method == CLI_COPY_METHOD {
                                    match serde_json::from_value::<CliCopyRequest>(request.params) {
                                        Ok(log) => {
                                            cli_copy_log(&client, log).await.map(Value::String)
                                        }
                                        Err(error) => Err(error.into()),
                                    }
                                } else {
                                    client.request(&request.method, request.params).await
                                };
                                if responses
                                    .send(Response {
                                        tag: request.tag,
                                        result,
                                    })
                                    .await
                                    .is_ok()
                                {
                                    waker.wake();
                                }
                            });
                        },
                        || waker.wake(),
                    )
                    .await;
                    let _ = client.close().await;
                    drop(responses);
                    waker.wake();
                });
            })
            .map_err(|e| AppError::new("runtime", e.to_string()))?;
        Ok(Self {
            commands,
            responses: receiver,
            thread: Some(worker),
        })
    }
    /// 非阻塞入队，队列已满或关闭时返回 ipc_queue 错误。
    pub fn request(&self, tag: impl Into<String>, method: &str, params: Value) -> Result<()> {
        self.commands
            .try_send(Request {
                tag: tag.into(),
                method: method.into(),
                params,
            })
            .map_err(|e| AppError::new("ipc_queue", e.to_string()))
    }
    /// 在 IPC 后台读取完整缓存，不重新访问 Redis。
    pub(super) fn redis_selected_rows(
        &self,
        tag: String,
        rows: Vec<Vec<DataValue>>,
        kind: Option<&str>,
    ) -> Result<()> {
        self.request(tag, REDIS_ROWS_METHOD, json!({"rows":rows,"kind":kind}))
    }

    pub(super) fn copy_redis_value(&self, tag: String, result_id: Uuid, kind: &str) -> Result<()> {
        self.request(
            tag,
            REDIS_COPY_METHOD,
            json!({"result_id":result_id,"kind":kind}),
        )
    }
    /// 非阻塞读取；None 可能表示暂时为空，须另用 is_closed 判断关闭。
    pub fn receive(&mut self) -> Option<Response> {
        self.responses.try_recv().ok()
    }
    /// 等待下一个响应；发送端全部关闭且队列耗尽时返回 None。
    pub fn receive_blocking(&mut self) -> Option<Response> {
        self.responses.blocking_recv()
    }
    pub fn is_closed(&self) -> bool {
        self.responses.is_closed()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // 先关闭接收端，使等待写入已满响应队列的任务能够退出。
        self.responses.close();
        let (replacement, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.commands, replacement));
        // 数据库请求可能仍在运行，不能在 UI 线程等待后台线程结束。
        let _ = self.thread.take();
    }
}

#[cfg(test)]
#[path = "../tests/unit/backend.rs"]
mod tests;

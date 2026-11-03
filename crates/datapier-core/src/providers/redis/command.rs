use super::*;

#[cfg(test)]
#[path = "../../../tests/unit/redis/command.rs"]
mod tests;

impl RedisSession {
    pub(super) async fn run(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let mut redis_results = Vec::new();
        let result = self.run_commands(p, sink, cancel, &mut redis_results).await;
        let command_db = if self.command_dead {
            None
        } else {
            Some(self.command_db)
        };
        match result {
            Ok(mut value) => {
                value["command_db"] = json!(command_db);
                value["redis_results"] = json!(redis_results);
                Ok(value)
            }
            Err(mut error) => {
                let mut details = match *error.details {
                    Json::Object(map) => Json::Object(map),
                    Json::Null => json!({}),
                    other => json!({"cause":other}),
                };
                details["command_db"] = json!(command_db);
                details["redis_results"] = json!(redis_results);
                error.details = Box::new(details);
                Err(error)
            }
        }
    }

    async fn run_commands(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        redis_results: &mut Vec<Json>,
    ) -> Result<Json> {
        let text = param::<String>(p, "text")?;
        let commands = parse_commands(&text)?;
        if commands.is_empty() {
            return Err(AppError::invalid("命令为空"));
        }
        for a in &commands {
            if a[0].eq_ignore_ascii_case(b"AUTH") {
                return Err(AppError::invalid("请通过连接设置进行 Redis 认证"));
            }
            let name = String::from_utf8_lossy(&a[0]).to_ascii_uppercase();
            if matches!(
                name.as_str(),
                "SUBSCRIBE"
                    | "PSUBSCRIBE"
                    | "SSUBSCRIBE"
                    | "MONITOR"
                    | "SYNC"
                    | "PSYNC"
                    | "QUIT"
                    | "RESET"
                    | "HELLO"
            ) {
                return Err(AppError::invalid("此连接模式不受支持"));
            }
            if self.profile.access_mode == AccessMode::ReadOnly && !read_command(a) {
                return Err(AppError::new(
                    "permission_denied",
                    format!("只读连接不允许命令 {name}"),
                ));
            }
        }
        if self.command_dead {
            return Err(AppError::invalid("命令连接已失效，请重新连接"));
        }
        // 命令连接单独建立：用户命令可能修改 SELECT 或事务等连接状态。
        if self.command_conn.is_none() {
            self.command_conn = Some(tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AppError::cancelled()),
                result = connect_database(&self.client, &self.profile, self.secret.as_deref()) => result?,
            });
        }
        let mut results = Vec::new();
        for a in commands {
            // SELECT 的回复属于发送前的 DB；之后的命令使用成功选择的新 DB。
            let result_db = self.command_db;
            ensure_not_cancelled(cancel)?;
            let may_write = !read_command(&a);
            sink.mark_sent(may_write).await?;
            let mut command = redis::cmd(
                std::str::from_utf8(&a[0]).map_err(|_| AppError::invalid("命令名必须是 UTF-8"))?,
            );
            for v in &a[1..] {
                command.arg(v);
            }
            // 停止等待后丢弃连接；已发送的写命令可能仍在服务端执行。
            let mut conn = self.command_conn.take().unwrap();
            let response = tokio::select! { result=redis::aio::ConnectionLike::req_packed_command(&mut conn,&command)=>result, _=cancel.cancelled()=>{self.command_dead=true; return Err(if may_write {AppError::unknown("已发送命令，停止等待后写入结果未知")} else {AppError::cancelled()});} };
            let value = match response {
                Ok(redis::Value::ServerError(e)) => {
                    if a[0].eq_ignore_ascii_case(b"EXEC") {
                        self.command_transaction = None;
                    }
                    self.command_conn = Some(conn);
                    return Err(AppError::new("database", format!("{e:?}")));
                }
                Ok(v) => v,
                Err(e) => {
                    if !e.is_io_error() {
                        if a[0].eq_ignore_ascii_case(b"EXEC") {
                            self.command_transaction = None;
                        }
                        self.command_conn = Some(conn);
                        return Err(e.into());
                    }
                    self.command_dead = true;
                    return Err(if may_write {
                        error_unknown(e)
                    } else {
                        e.into()
                    });
                }
            };
            // MULTI 中 SELECT 先入队，只有 EXEC 对应项成功才改变实际 DB。
            let select_db = if a[0].eq_ignore_ascii_case(b"SELECT") && a.len() == 2 {
                std::str::from_utf8(&a[1])
                    .ok()
                    .and_then(|db| db.parse::<u32>().ok())
            } else {
                None
            };
            if a[0].eq_ignore_ascii_case(b"MULTI") {
                self.command_transaction = Some(Vec::new());
            } else if a[0].eq_ignore_ascii_case(b"DISCARD") {
                self.command_transaction = None;
            } else if a[0].eq_ignore_ascii_case(b"EXEC") {
                if let (Some(queued), redis::Value::Array(replies)) =
                    (self.command_transaction.take(), &value)
                {
                    for (db, reply) in queued.into_iter().zip(replies) {
                        if let Some(db) = db
                            && !matches!(reply, redis::Value::ServerError(_))
                        {
                            self.command_db = db;
                        }
                    }
                }
            } else if let Some(queued) = &mut self.command_transaction {
                queued.push(select_db);
            } else if let Some(db) = select_db {
                self.command_db = db;
            }
            self.command_conn = Some(conn);
            let id = sink
                .start(vec![Column::new("reply", "redis")], "redis_command")
                .await?;
            redis_results.push(json!({"result_id":id,"redis_db":result_db}));
            sink.append(id, vec![vec![Value::Json(redis_value(value))]])
                .await?;
            sink.finish(id, json!({"complete":true,"redis_db":result_db}))
                .await?;
            results.push(id);
        }
        Ok(json!({"results":results}))
    }
}
pub(crate) fn read_command(a: &[Vec<u8>]) -> bool {
    let n = String::from_utf8_lossy(&a[0]).to_ascii_uppercase();
    matches!(
        n.as_str(),
        "PING"
            | "ECHO"
            | "GET"
            | "MGET"
            | "GETRANGE"
            | "STRLEN"
            | "EXISTS"
            | "TYPE"
            | "TTL"
            | "PTTL"
            | "SCAN"
            | "HSCAN"
            | "SSCAN"
            | "ZSCAN"
            | "HGET"
            | "HMGET"
            | "HGETALL"
            | "HEXISTS"
            | "HLEN"
            | "HKEYS"
            | "HVALS"
            | "HSTRLEN"
            | "LLEN"
            | "LINDEX"
            | "LRANGE"
            | "LPOS"
            | "SCARD"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SMEMBERS"
            | "SRANDMEMBER"
            | "SDIFF"
            | "SINTER"
            | "SUNION"
            | "ZCARD"
            | "ZSCORE"
            | "ZMSCORE"
            | "ZRANGE"
            | "ZREVRANGE"
            | "ZRANGEBYSCORE"
            | "ZREVRANGEBYSCORE"
            | "ZRANGEBYLEX"
            | "ZREVRANGEBYLEX"
            | "ZCOUNT"
            | "ZLEXCOUNT"
            | "ZRANK"
            | "ZREVRANK"
            | "INFO"
            | "DBSIZE"
            | "TIME"
            | "SELECT"
    )
}
pub(crate) fn parse_commands(text: &str) -> Result<Vec<Vec<Vec<u8>>>> {
    let input = text.as_bytes();
    let mut all = Vec::new();
    let mut line = Vec::new();
    let mut word = Vec::new();
    let mut active = false;
    let mut quote = 0u8;
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        i += 1;
        if quote == 0 && (b == b' ' || b == b'\t' || b == b'\r' || b == b'\n') {
            if active {
                line.push(std::mem::take(&mut word));
                active = false;
            }
            if b == b'\n' && !line.is_empty() {
                all.push(std::mem::take(&mut line));
            }
            continue;
        }
        if b == b'\'' || b == b'"' {
            if quote == 0 {
                quote = b;
                active = true;
                continue;
            }
            if quote == b {
                quote = 0;
                continue;
            }
        }
        if b == b'\\' {
            if i >= input.len() {
                return Err(AppError::invalid("转义不完整"));
            }
            let c = input[i];
            i += 1;
            if quote == b'\'' && c != b'\'' {
                word.push(b'\\');
                word.push(c);
            } else if c == b'x' {
                if i + 1 >= input.len() {
                    return Err(AppError::invalid("十六进制转义不完整"));
                }
                let h = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                let v = h(input[i])
                    .zip(h(input[i + 1]))
                    .ok_or_else(|| AppError::invalid("无效十六进制转义"))?;
                word.push(v.0 * 16 + v.1);
                i += 2;
            } else {
                word.push(match c {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 8,
                    b'a' => 7,
                    _ => c,
                });
            }
            active = true;
            continue;
        }
        word.push(b);
        active = true;
    }
    if quote != 0 {
        return Err(AppError::invalid("引号未闭合"));
    }
    if active {
        line.push(word);
    }
    if !line.is_empty() {
        all.push(line);
    }
    Ok(all)
}

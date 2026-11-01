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
        let text = param::<String>(p, "text")?;
        let commands = parse_commands(&text)?;
        if commands.is_empty() {
            return Err(AppError::invalid("命令为空"));
        }
        for a in &commands {
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
            self.command_conn =
                Some(connect_database(&self.client, &self.profile, self.secret.as_deref()).await?);
        }
        let mut results = Vec::new();
        for a in commands {
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
                    self.command_conn = Some(conn);
                    return Err(AppError::new("database", format!("{e:?}")));
                }
                Ok(v) => v,
                Err(e) => {
                    self.command_dead = true;
                    return Err(if may_write {
                        error_unknown(e)
                    } else {
                        e.into()
                    });
                }
            };
            self.command_conn = Some(conn);
            let id = sink
                .start(vec![Column::new("reply", "redis")], "redis_command")
                .await?;
            sink.append(id, vec![vec![Value::Json(redis_value(value))]])
                .await?;
            sink.finish(id, json!({"complete":true})).await?;
            results.push(id);
        }
        Ok(json!({"results":results}))
    }
}
fn read_command(a: &[Vec<u8>]) -> bool {
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
fn parse_commands(text: &str) -> Result<Vec<Vec<Vec<u8>>>> {
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

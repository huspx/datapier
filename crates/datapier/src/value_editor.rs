//! 在独立终端中消费一次性交接并运行用户配置的编辑器。
use datapier_core::{AppError, Result, model::ClientKind};
use datapier_engine::{Client, ClientOptions};
use serde_json::json;
use std::{path::PathBuf, process::Stdio};

/// 连接已有 Engine，领取一次性交接；编辑器退出只报告终态，不回填数据。
pub async fn run(root: PathBuf, handoff: String) -> Result<()> {
    // handoff 由 Engine 生成，拒绝非 UUID 输入，避免将任意文本传入内部协议。
    if handoff.len() != 36
        || !handoff
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(AppError::invalid("无效编辑器交接 ID"));
    }
    let client = Client::connect_with(
        &root,
        ClientOptions {
            kind: ClientKind::ValueEditor,
            start_engine: false,
            ..ClientOptions::default()
        },
    )
    .await?;
    let claim = client
        .request("value.editor.claim", json!({"handoff": handoff}))
        .await?;
    let program = claim
        .get("program")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::invalid("编辑器交接缺少程序"))?;
    let args: Vec<String> = serde_json::from_value(
        claim
            .get("args")
            .cloned()
            .ok_or_else(|| AppError::invalid("编辑器交接缺少参数"))?,
    )?;
    let result = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await;
    let (status, error) = match result {
        Ok(status) => (
            json!({"success":status.success(),"code":status.code()}),
            None,
        ),
        Err(error) => (json!({"success":false,"code":null}), Some(error)),
    };
    client
        .request(
            "value.editor.finish",
            json!({"handoff":handoff,"status":status}),
        )
        .await?;
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}

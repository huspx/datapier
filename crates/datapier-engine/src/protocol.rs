//! UDS 与 Named Pipe 共用的长度前缀 JSON 帧，不以换行或一次读取划分消息。

use datapier_core::{AppError, Result};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// 与协议 v1 的 8 MiB 上限一致；必须在按不可信长度分配缓冲区之前拒绝超限帧。
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// 读取一帧，先验证大端 u32 长度，再完整读取并解析 UTF-8 JSON。
///
/// 此操作不保证取消安全：事件分发期间应保留同一个读取 future；中途丢弃后
/// 不能从当前位置重新读帧头，否则已消费的前缀会使后续消息错位。
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(AppError::invalid("IPC 帧长度无效"));
    }
    let mut data = vec![0; size];
    reader.read_exact(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
}
/// 写入完整 JSON 帧并 flush；超限检查在写帧头前完成，调用方可改发错误响应。
pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_FRAME {
        return Err(AppError::invalid("IPC 响应超过 8 MiB"));
    }
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}

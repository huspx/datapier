use crate::model::AppError;
use sqlx::mysql::MySqlDatabaseError;

// 保留 MySQL errno 和 SQLSTATE，调用方据此区分服务端中断与未知写入结果。
pub(super) fn mysql_error(e: sqlx::Error) -> AppError {
    let details = e
        .as_database_error()
        .and_then(|error| error.try_downcast_ref::<MySqlDatabaseError>())
        .map(|error| {
            serde_json::json!({
                "mysql_errno": error.number(),
                "sqlstate": error.code(),
            })
        });
    let mut result = AppError::from(e);
    if details.as_ref().and_then(|d| d["mysql_errno"].as_u64()) == Some(3159) {
        result.code = "tls_required".into();
        result.message = format!("服务器要求启用 TLS: {}", result.message);
    }
    if let Some(details) = details {
        result.details = Box::new(details);
    }
    result
}

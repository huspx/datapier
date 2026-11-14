//! 数据库工作台的类型、执行、持久化与内置 provider；不依赖界面或 IPC。

pub mod execution;
pub mod model;
pub mod policy;
pub mod providers;
pub mod storage;
pub mod value;
pub use execution::ExecutionSink;
pub use model::{AppError, Result};
pub use storage::Store;

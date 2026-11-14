//! 固定数量的配置连接，不在连接丢失后重新解析数据目录。
use crate::model::{AppError, Result};
use sqlx::{Connection, SqliteConnection};
use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

struct State {
    available: Vec<SqliteConnection>,
    leased: usize,
    closed: bool,
}
struct Inner {
    state: Mutex<State>,
    permits: Arc<Semaphore>,
    returned: Notify,
}

/// 只使用初始化时打开的连接；损坏的连接不会被替换。
#[derive(Clone)]
pub struct FixedPool(Arc<Inner>);

pub struct FixedConnection {
    pool: Arc<Inner>,
    connection: Option<SqliteConnection>,
    permit: Option<OwnedSemaphorePermit>,
}

impl FixedPool {
    pub fn new(connections: Vec<SqliteConnection>) -> Self {
        let count = connections.len();
        let permits = Arc::new(Semaphore::new(count));
        if count == 0 {
            permits.close();
        }
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                available: connections,
                leased: 0,
                closed: false,
            }),
            permits,
            returned: Notify::new(),
        }))
    }

    pub async fn acquire(&self) -> Result<FixedConnection> {
        let permit = self
            .0
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::new("storage", "配置连接池已关闭或连接已失效"))?;
        let connection = {
            let mut state = self.0.state.lock().unwrap();
            if state.closed {
                return Err(AppError::new("storage", "配置连接池已关闭"));
            }
            let connection = state
                .available
                .pop()
                .ok_or_else(|| AppError::new("storage", "配置连接池状态无效"))?;
            state.leased += 1;
            connection
        };
        let mut lease = FixedConnection {
            pool: self.0.clone(),
            connection: Some(connection),
            permit: Some(permit),
        };
        if lease.ping().await.is_err() {
            lease.connection.take();
            if let Some(permit) = lease.permit.take() {
                permit.forget();
            }
            // 固定集合不能补连；首个损坏即关闭借用入口，避免最后几个连接
            // 并发失效时留下零 permit 却仍开放的信号量，令后续请求无限等待。
            self.0.state.lock().unwrap().closed = true;
            self.0.permits.close();
            return Err(AppError::new("storage", "配置连接已失效"));
        }
        Ok(lease)
    }

    pub async fn close(&self) {
        {
            let mut state = self.0.state.lock().unwrap();
            state.closed = true;
            self.0.permits.close();
        }
        loop {
            let notified = self.0.returned.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let connections = {
                let mut state = self.0.state.lock().unwrap();
                if state.leased == 0 {
                    Some(std::mem::take(&mut state.available))
                } else {
                    None
                }
            };
            if let Some(connections) = connections {
                for connection in connections {
                    let _ = connection.close().await;
                }
                return;
            }
            notified.await;
        }
    }
}
impl Deref for FixedConnection {
    type Target = SqliteConnection;
    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("连接已归还")
    }
}
impl DerefMut for FixedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("连接已归还")
    }
}
impl Drop for FixedConnection {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().unwrap();
        if let Some(connection) = self.connection.take() {
            state.available.push(connection);
        }
        state.leased -= 1;
        drop(state);
        self.pool.returned.notify_waiters();
    }
}

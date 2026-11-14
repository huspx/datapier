//! 编辑计划与值缓冲复用结果注册表的客户端额度，不建立第二份内存预算。
use super::*;

fn used_bytes(state: &ResultStore, owner: Uuid, except: Option<Uuid>) -> Result<usize> {
    let results = state
        .entries
        .values()
        .filter(|result| result.owner == owner)
        .map(|result| result.bytes);
    let auxiliary = state
        .auxiliary
        .iter()
        .filter(|(id, (holder, _))| *holder == owner && Some(**id) != except)
        .map(|(_, (_, bytes))| *bytes);
    results.chain(auxiliary).try_fold(0usize, |used, bytes| {
        used.checked_add(bytes)
            .ok_or_else(|| AppError::new("resource_limit", "客户端内存记账溢出"))
    })
}

impl Store {
    /// 为没有结果 execution 的编辑句柄保留额度；同一 handle 更新是原子替换。
    pub async fn reserve_auxiliary_for_owner(
        &self,
        owner: Uuid,
        handle: Uuid,
        bytes: usize,
    ) -> Result<()> {
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut state = self.results.lock().await;
        if state
            .auxiliary
            .get(&handle)
            .is_some_and(|(holder, _)| *holder != owner)
        {
            return Err(AppError::new("permission_denied", "内存句柄属于其他客户端"));
        }
        if used_bytes(&state, owner, Some(handle))?
            .checked_add(bytes)
            .is_none_or(|used| used > budget)
        {
            return Err(AppError::new("resource_limit", "客户端内存预算已用尽"));
        }
        state.auxiliary.insert(handle, (owner, bytes));
        Ok(())
    }

    /// 返回结果、扫描、计划和单值缓冲共同记账后的剩余额度。
    pub async fn available_memory_for_owner(&self, owner: Uuid) -> Result<usize> {
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let state = self.results.lock().await;
        Ok(budget.saturating_sub(used_bytes(&state, owner, None)?))
    }
}

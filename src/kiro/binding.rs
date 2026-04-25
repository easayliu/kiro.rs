//! 用户 → 凭证 粘性绑定表（内存）
//!
//! 用途：跨凭证场景下，让同一用户的请求持续落在同一个上游凭证，
//! 避免上游 prompt cache 在每个凭证上反复预热造成成本放大。
//!
//! key 使用 `binding_key`（cache_tracker 从 metadata.user_id 的 device_id +
//! account_uuid 提取的 SHA256[0..8]，刻意不含 session_id），value 为
//! credential_id。粒度比 cache 分桶的 `identity_key` 粗一档，同一设备同账号
//! 跨 session 的请求会继续绑到原凭证，复用稳定公共前缀（system prompt /
//! tools / machine_id）的上游缓存。绑定状态仅在内存中维护，进程重启后全部清空。

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 错误计数滑动窗口长度
const ERROR_WINDOW: Duration = Duration::from_secs(60);

/// 窗口内累计多少次可计数错误触发 rebind
const REBIND_THRESHOLD: usize = 3;

/// 绑定记录
#[derive(Debug, Clone)]
struct Binding {
    credential_id: u64,
    last_seen: Instant,
    rebind_count: u32,
}

struct BindingState {
    /// identity_key → binding
    bindings: HashMap<u64, Binding>,
    /// credential_id → 近期错误时间戳（滑动窗口）
    cred_errors: HashMap<u64, VecDeque<Instant>>,
}

/// 用户级凭证粘性绑定表。
pub struct BindingTable {
    inner: Mutex<BindingState>,
}

impl Default for BindingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BindingTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BindingState {
                bindings: HashMap::new(),
                cred_errors: HashMap::new(),
            }),
        }
    }

    /// 查询或创建绑定。返回应使用的 credential_id。
    ///
    /// - 已有绑定且凭证在 `available` 中：原样返回并刷新 `last_seen`
    /// - 已有绑定但凭证已不可用（被禁用/删除）：静默改绑到最空凭证
    /// - 无绑定：在 `available` 中挑绑定用户数最少的创建新绑定
    /// - `available` 为空：返回 None，由调用方走默认选择
    pub fn resolve(&self, identity_key: u64, available: &[u64]) -> Option<u64> {
        if available.is_empty() {
            return None;
        }
        let mut state = self.inner.lock();
        let now = Instant::now();

        if let Some(b) = state.bindings.get_mut(&identity_key) {
            if available.contains(&b.credential_id) {
                b.last_seen = now;
                return Some(b.credential_id);
            }
        }

        let picked = pick_least_bound(&state.bindings, available)?;
        let rebind_count = state
            .bindings
            .get(&identity_key)
            .map(|b| b.rebind_count)
            .unwrap_or(0);
        state.bindings.insert(
            identity_key,
            Binding {
                credential_id: picked,
                last_seen: now,
                rebind_count,
            },
        );
        Some(picked)
    }

    /// 记录一次上游错误。返回 true 表示该凭证 1 分钟内累计错误已达阈值，
    /// 调用方应对相关用户触发 `rebind`。
    ///
    /// 只应对"值得触发改绑"的错误计数，典型是长 retry-after 的 429、
    /// 配额耗尽、连续 5xx 等。短时 429（retry-after 很小）不要调用，
    /// 否则会把瞬态抖动放大成绑定漂移。
    pub fn report_error(&self, credential_id: u64) -> bool {
        let now = Instant::now();
        let mut state = self.inner.lock();
        let dq = state.cred_errors.entry(credential_id).or_default();
        while let Some(front) = dq.front() {
            if now.duration_since(*front) > ERROR_WINDOW {
                dq.pop_front();
            } else {
                break;
            }
        }
        dq.push_back(now);
        dq.len() >= REBIND_THRESHOLD
    }

    /// 把用户从 `avoid` 凭证迁到 `available` 中的另一个凭证。
    /// 返回新凭证 id。`available` 可以包含 `avoid`，本函数会自动排除。
    ///
    /// 幂等性：若用户当前绑定已经不是 `avoid`（其他并发调用已迁过），
    /// 直接返回当前绑定凭证，不再改动，避免同一用户被反复挪位。
    pub fn rebind(&self, identity_key: u64, avoid: u64, available: &[u64]) -> Option<u64> {
        let candidates: Vec<u64> = available.iter().copied().filter(|&c| c != avoid).collect();
        if candidates.is_empty() {
            return None;
        }
        let mut state = self.inner.lock();
        // 守卫：若别的线程已把该用户迁离 `avoid`，本次视作 no-op 返回现状。
        // 否则两个并发 report_error 都看到阈值达标，会各自独立触发 rebind，
        // 把用户挪两次（第二次是无意义的 churn）。
        if let Some(existing) = state.bindings.get(&identity_key) {
            if existing.credential_id != avoid {
                return Some(existing.credential_id);
            }
        }
        let now = Instant::now();
        let picked = pick_least_bound(&state.bindings, &candidates)?;
        let entry = state.bindings.entry(identity_key).or_insert(Binding {
            credential_id: picked,
            last_seen: now,
            rebind_count: 0,
        });
        entry.credential_id = picked;
        entry.last_seen = now;
        entry.rebind_count = entry.rebind_count.saturating_add(1);
        state.cred_errors.remove(&avoid);
        Some(picked)
    }

    /// 清理长时间未活跃的绑定。定期由后台任务调用即可，不调用也不会崩，
    /// 只是内存会随独立用户数线性增长。
    #[allow(dead_code)]
    pub fn sweep_stale(&self, max_idle: Duration) {
        let now = Instant::now();
        let mut state = self.inner.lock();
        state
            .bindings
            .retain(|_, b| now.duration_since(b.last_seen) <= max_idle);
    }

    /// 当前绑定条数（观测/测试用）
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().bindings.len()
    }
}

/// 在 `available` 中选择当前被绑用户数最少的凭证。
/// 平局时按 `available` 顺序取靠前的，保证确定性。
fn pick_least_bound(bindings: &HashMap<u64, Binding>, available: &[u64]) -> Option<u64> {
    if available.is_empty() {
        return None;
    }
    let mut counts: HashMap<u64, usize> = available.iter().map(|&c| (c, 0usize)).collect();
    for b in bindings.values() {
        if let Some(c) = counts.get_mut(&b.credential_id) {
            *c += 1;
        }
    }
    available
        .iter()
        .min_by_key(|c| counts.get(c).copied().unwrap_or(0))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_creates_new_binding() {
        let table = BindingTable::new();
        let picked = table.resolve(42, &[1, 2, 3]).unwrap();
        assert!([1, 2, 3].contains(&picked));
        assert_eq!(table.resolve(42, &[1, 2, 3]), Some(picked));
    }

    #[test]
    fn resolve_returns_none_when_no_credentials() {
        let table = BindingTable::new();
        assert_eq!(table.resolve(42, &[]), None);
    }

    #[test]
    fn resolve_silently_rebinds_when_credential_unavailable() {
        let table = BindingTable::new();
        let first = table.resolve(42, &[1]).unwrap();
        assert_eq!(first, 1);
        // 凭证 1 被移除，必须改绑到 2
        let second = table.resolve(42, &[2]).unwrap();
        assert_eq!(second, 2);
    }

    #[test]
    fn least_bound_picks_empty_credential() {
        let table = BindingTable::new();
        // 把 3 个用户都绑到 1
        for uid in [10, 11, 12] {
            table.resolve(uid, &[1]).unwrap();
        }
        // 新用户来，凭证 1 和 2 都可用，应挑 2
        let picked = table.resolve(99, &[1, 2]).unwrap();
        assert_eq!(picked, 2);
    }

    #[test]
    fn report_error_triggers_rebind_after_threshold() {
        let table = BindingTable::new();
        assert!(!table.report_error(7));
        assert!(!table.report_error(7));
        assert!(table.report_error(7));
    }

    #[test]
    fn rebind_moves_user_off_failing_credential() {
        let table = BindingTable::new();
        assert_eq!(table.resolve(42, &[1]), Some(1));
        let new_cred = table.rebind(42, 1, &[1, 2]).unwrap();
        assert_eq!(new_cred, 2);
        assert_eq!(table.resolve(42, &[1, 2]), Some(2));
    }

    #[test]
    fn rebind_is_idempotent_when_user_already_moved() {
        // 回归：并发 report_error 可能让两个线程都判定需要 rebind。
        // 第二次 rebind 时用户已经迁出 `avoid`，必须 no-op，不然会被无意义
        // 挪到第三个凭证。
        let table = BindingTable::new();
        table.resolve(42, &[1]).unwrap();
        let first = table.rebind(42, 1, &[1, 2, 3]).unwrap();
        assert_eq!(first, 2);
        // 第二次用相同 avoid=1 调用 rebind：用户已经在 2 上，不能再挪到 3
        let second = table.rebind(42, 1, &[1, 2, 3]).unwrap();
        assert_eq!(second, 2);
    }

    #[test]
    fn rebind_returns_none_when_no_alternative() {
        let table = BindingTable::new();
        table.resolve(42, &[1]).unwrap();
        assert_eq!(table.rebind(42, 1, &[1]), None);
    }

    #[test]
    fn sweep_stale_removes_idle_bindings() {
        let table = BindingTable::new();
        table.resolve(42, &[1]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        table.sweep_stale(Duration::from_millis(10));
        assert_eq!(table.len(), 0);
    }
}

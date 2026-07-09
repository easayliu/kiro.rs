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
//!
//! 粘性只在缓存仍温热时有价值：空闲超过 [`COLD_IDLE`]（上游缓存最长 TTL + 余量）
//! 的绑定视为冷绑定，回流时按当前负载重新放置、计数时不算占用——改绑此时零成本，
//! 借身份的自然回流持续把负载从热点凭证摊开（免费再均衡）。
//!
//! 放置决策是负载感知的：候选按 (外部负载, fresh 绑定数, 候选顺序) 字典序取最小，
//! 外部负载由调用方传入（生产为凭证 60s RPM 窗口计数，与限流记账同源）。
//!
//! 鲸鱼分片：单身份速率达到 [`WHALE_RPM_THRESHOLD`] 后绑定升级为多凭证分片，
//! 分片内按会话级 shard_key 做 rendezvous 路由——同一 session 恒定落同一成员
//! （保缓存局部性），不同 session 摊到不同成员（拆热点）。见 [`BindingTable::resolve`]。

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 错误计数滑动窗口长度
const ERROR_WINDOW: Duration = Duration::from_secs(60);

/// 窗口内累计多少次可计数错误触发 rebind
const REBIND_THRESHOLD: usize = 3;

/// 绑定空闲超过该时长视为"缓存已冷"：上游 prompt cache 最长 TTL 为 1 小时
/// （见 middleware 的 DEFAULT_PROMPT_CACHE_TTL_SECS），空闲超过它意味着该身份
/// 在原凭证上已无任何可复用缓存，此时改绑零成本（免费再均衡）：
/// - `resolve` 遇到冷绑定不再原样返回，按当前负载重新放置到最空凭证；
/// - `pick_least_bound` 计数时忽略冷绑定，它们不代表真实占用。
/// 阈值取 TTL + 15 分钟余量，确保"冷"判定不会误伤仍在复用缓存的活跃身份。
const COLD_IDLE: Duration = Duration::from_secs(75 * 60);

/// 鲸鱼判定阈值：单身份 60s 窗口内请求数达到该值视为鲸鱼流量。
/// 交互型单人客户端通常 <10 RPM；达到 30 基本可断定是聚合/自动化流量，
/// 单凭证独扛会成为热点，允许其分片到多个凭证摊热。
const WHALE_RPM_THRESHOLD: usize = 30;

/// 鲸鱼分片数上限。每多一片，该身份的公共前缀就要在新成员上多预热一份缓存，
/// 上限控制预热成本：再热的身份最多摊到 4 个凭证。
const MAX_WHALE_SHARDS: usize = 4;

/// 身份请求速率滑动窗口长度（鲸鱼判定用）
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// 绑定记录
#[derive(Debug, Clone)]
struct Binding {
    /// 主凭证；未分片时即唯一凭证。分片后恒为 `shards[0]` 语义上的一员
    /// （不变式：`shards` 为空，或 `shards` 包含 `credential_id`）。
    credential_id: u64,
    /// 鲸鱼分片集（含主凭证）。空 = 未分片（常规身份）。
    /// 只增不缩：主动收缩会造成路由震荡，冷透回流时整体重置为单凭证。
    shards: Vec<u64>,
    /// 60s 请求时间戳窗口，用于鲸鱼判定（每次 resolve 记一笔）
    req_window: VecDeque<Instant>,
    last_seen: Instant,
    rebind_count: u32,
}

impl Binding {
    /// 当前生效的凭证集：未分片时为主凭证单元素集
    fn effective(&self) -> Vec<u64> {
        if self.shards.is_empty() {
            vec![self.credential_id]
        } else {
            self.shards.clone()
        }
    }
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
    /// 冷绑定判定阈值（生产恒为 [`COLD_IDLE`]，测试注入小值）
    cold_idle: Duration,
}

impl Default for BindingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BindingTable {
    pub fn new() -> Self {
        Self::with_cold_idle(COLD_IDLE)
    }

    fn with_cold_idle(cold_idle: Duration) -> Self {
        Self {
            inner: Mutex::new(BindingState {
                bindings: HashMap::new(),
                cred_errors: HashMap::new(),
            }),
            cold_idle,
        }
    }

    /// 查询或创建绑定。返回应使用的 credential_id。
    ///
    /// `available` 为 (credential_id, 外部负载) 列表，负载用于放置与扩片决策
    /// （生产传凭证 60s RPM 窗口计数）。`shard_key` 是会话级分流键（比
    /// identity_key 细一档，含 session），仅在鲸鱼分片后参与路由。
    ///
    /// - 已有绑定、凭证在 `available` 中且未冷：粘住不动，刷新 `last_seen`；
    ///   已分片的鲸鱼按 shard_key 在分片内 rendezvous 路由
    /// - 速率达到 [`WHALE_RPM_THRESHOLD`]：升级为鲸鱼，逐步扩片
    ///   （每次 resolve 最多扩一片，新成员按负载最低选）
    /// - 已有绑定但凭证已不可用（被禁用/删除）：分片成员被剔除、主凭证由存活
    ///   成员接任；全员不可用则静默重放置
    /// - 已有绑定但空闲超过 [`COLD_IDLE`]（上游缓存必然已过期）：重置鲸鱼状态，
    ///   视同新身份按当前负载重新放置——粘住原凭证已无缓存收益，重新放置零成本，
    ///   借身份回流把负载从热点凭证摊开（免费再均衡）
    /// - 无绑定：按 (负载, fresh 绑定数, 候选顺序) 放置到最空凭证
    /// - `available` 为空：返回 None，由调用方走默认选择
    pub fn resolve(&self, identity_key: u64, shard_key: u64, available: &[(u64, usize)]) -> Option<u64> {
        if available.is_empty() {
            return None;
        }
        let mut guard = self.inner.lock();
        let state = &mut *guard;
        let now = Instant::now();
        let in_avail = |id: u64| available.iter().any(|&(c, _)| c == id);

        /// 阶段一的判定结果：扩片/重放置需要统计全表占用（与单条绑定的可变
        /// 借用冲突），故先短借用出结论，阶段二再执行。
        enum Next {
            /// 绑定健康：直接路由到该凭证
            Route(u64),
            /// 鲸鱼速率超过当前分片承载：本次扩一片后再路由
            Expand,
            /// 无绑定 / 冷透 / 凭证全员不可用：重新放置（携带原 rebind_count）
            Place(u32),
        }

        let next = match state.bindings.get_mut(&identity_key) {
            None => Next::Place(0),
            Some(b) if now.duration_since(b.last_seen) > self.cold_idle => {
                // 冷透：缓存已无可复用，鲸鱼状态一并重置，按当前负载重放置
                Next::Place(b.rebind_count)
            }
            Some(b) => {
                // 速率记账（鲸鱼判定窗口）
                while b
                    .req_window
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
                {
                    b.req_window.pop_front();
                }
                b.req_window.push_back(now);

                // 剔除已不可用的分片成员；只剩一个成员时退化为未分片
                b.shards.retain(|&s| in_avail(s));
                if b.shards.len() == 1 {
                    b.credential_id = b.shards[0];
                    b.shards.clear();
                }
                // 主凭证不可用：由存活分片成员接任；无存活者走重放置
                if !in_avail(b.credential_id) {
                    match b.shards.first() {
                        Some(&survivor) => b.credential_id = survivor,
                        None => {}
                    }
                }

                if in_avail(b.credential_id) {
                    b.last_seen = now;
                    let target = whale_target(b.req_window.len()).min(available.len());
                    if target > b.shards.len().max(1) {
                        Next::Expand
                    } else {
                        Next::Route(route_shard(&b.shards, b.credential_id, shard_key))
                    }
                } else {
                    Next::Place(b.rebind_count)
                }
            }
        };

        match next {
            Next::Route(id) => Some(id),
            Next::Expand => {
                // 新成员排除现有分片成员后按 (负载, fresh 绑定数, 顺序) 取最小。
                // 候选耗尽（全是现有成员）则本轮不扩，按现有分片路由。
                let members = state
                    .bindings
                    .get(&identity_key)
                    .expect("Expand 仅由已有绑定产生")
                    .effective();
                let candidates: Vec<(u64, usize)> = available
                    .iter()
                    .filter(|&&(c, _)| !members.contains(&c))
                    .copied()
                    .collect();
                let picked = pick_least_loaded(&state.bindings, &candidates, now, self.cold_idle);
                let b = state
                    .bindings
                    .get_mut(&identity_key)
                    .expect("Expand 仅由已有绑定产生");
                if let Some(new_member) = picked {
                    if b.shards.is_empty() {
                        b.shards.push(b.credential_id);
                    }
                    b.shards.push(new_member);
                    tracing::debug!(
                        identity = identity_key,
                        new_member,
                        shards = b.shards.len(),
                        rate_60s = b.req_window.len(),
                        "鲸鱼分片扩容：身份速率超过当前分片承载"
                    );
                }
                Some(route_shard(&b.shards, b.credential_id, shard_key))
            }
            Next::Place(rebind_count) => {
                let picked = pick_least_loaded(&state.bindings, available, now, self.cold_idle)?;
                state.bindings.insert(
                    identity_key,
                    Binding {
                        credential_id: picked,
                        shards: Vec::new(),
                        req_window: VecDeque::from([now]),
                        last_seen: now,
                        rebind_count,
                    },
                );
                Some(picked)
            }
        }
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
    /// 鲸鱼分片时 `avoid` 通常是某个分片成员：只替换该成员，其余成员保留
    /// （注意 rendezvous 只对"纯移除"保证健康成员的 session 不动，替换 =
    /// 移除+新增，健康成员上约 1/n 的 session 会按新打分重排到新成员——
    /// 这部分冷启不可避免，等价于摘掉坏成员后再扩容）；新成员同时排除现有
    /// 成员，避免换汤不换药或成员重复。未分片则整体迁移（原语义）。
    ///
    /// 幂等性：若用户当前生效凭证集已不含 `avoid`（其他并发调用已迁过），
    /// 直接返回当前主凭证，不再改动，避免同一用户被反复挪位。
    pub fn rebind(&self, identity_key: u64, avoid: u64, available: &[(u64, usize)]) -> Option<u64> {
        let mut guard = self.inner.lock();
        let state = &mut *guard;
        let now = Instant::now();

        // 守卫：若别的线程已把该用户迁离 `avoid`，本次视作 no-op 返回现状。
        // 否则两个并发 report_error 都看到阈值达标，会各自独立触发 rebind，
        // 把用户挪两次（第二次是无意义的 churn）。
        let members = match state.bindings.get(&identity_key) {
            Some(b) => {
                let eff = b.effective();
                if !eff.contains(&avoid) {
                    return Some(b.credential_id);
                }
                eff
            }
            None => Vec::new(),
        };

        let candidates: Vec<(u64, usize)> = available
            .iter()
            .filter(|&&(c, _)| c != avoid && !members.contains(&c))
            .copied()
            .collect();
        let picked = pick_least_loaded(&state.bindings, &candidates, now, self.cold_idle)?;

        let entry = state.bindings.entry(identity_key).or_insert(Binding {
            credential_id: picked,
            shards: Vec::new(),
            req_window: VecDeque::new(),
            last_seen: now,
            rebind_count: 0,
        });
        if entry.shards.is_empty() {
            entry.credential_id = picked;
        } else {
            // 分片绑定：只把出错成员换成新成员
            for s in entry.shards.iter_mut() {
                if *s == avoid {
                    *s = picked;
                }
            }
            if entry.credential_id == avoid {
                entry.credential_id = picked;
            }
        }
        entry.last_seen = now;
        entry.rebind_count = entry.rebind_count.saturating_add(1);
        state.cred_errors.remove(&avoid);
        Some(picked)
    }

    /// 清理长时间未活跃的绑定，返回移除的绑定条数。定期由后台任务调用，
    /// 否则 `bindings` 会随独立 device 数只增不减（device 绑定场景下每个唯一
    /// device_id 永久占一条）。
    ///
    /// 同时清理 `cred_errors` 中早已超出错误窗口的空队列，避免随历史出错凭据增长。
    pub fn sweep_stale(&self, max_idle: Duration) -> usize {
        let now = Instant::now();
        let mut state = self.inner.lock();
        let before = state.bindings.len();
        state
            .bindings
            .retain(|_, b| now.duration_since(b.last_seen) <= max_idle);
        // cred_errors：丢弃窗口外时间戳后为空的条目（凭据近期无错误则无需保留）
        state.cred_errors.retain(|_, dq| {
            while let Some(front) = dq.front() {
                if now.duration_since(*front) > ERROR_WINDOW {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            !dq.is_empty()
        });
        before - state.bindings.len()
    }

    /// 当前绑定条数（观测/测试用）
    pub fn len(&self) -> usize {
        self.inner.lock().bindings.len()
    }
}

/// 负载感知放置：在 `available`（(credential_id, 外部负载)）中选择放置目标。
///
/// 排序键 (外部负载, fresh 绑定占用数, 候选顺序)，字典序取最小：
/// - 外部负载是主信号，由调用方提供（生产为凭证 60s RPM 窗口计数，与限流
///   记账同源），鲸鱼流量再重也会被它如实反映；
/// - fresh 绑定数做平局裁决：低流量下负载普遍为 0 时退化为按绑定数均衡；
/// - 候选顺序兜底，保证确定性。
///
/// 空闲超过 `cold_idle` 的冷绑定不计占用（回流时会被重新放置）；
/// 分片绑定对每个成员各计 1（每个成员都真实承载该身份的一部分流量）。
fn pick_least_loaded(
    bindings: &HashMap<u64, Binding>,
    available: &[(u64, usize)],
    now: Instant,
    cold_idle: Duration,
) -> Option<u64> {
    if available.is_empty() {
        return None;
    }
    let mut bound_counts: HashMap<u64, usize> =
        available.iter().map(|&(c, _)| (c, 0usize)).collect();
    for b in bindings.values() {
        if now.duration_since(b.last_seen) > cold_idle {
            continue;
        }
        for member in b.effective() {
            if let Some(c) = bound_counts.get_mut(&member) {
                *c += 1;
            }
        }
    }
    available
        .iter()
        .enumerate()
        .min_by_key(|(idx, (c, load))| {
            (*load, bound_counts.get(c).copied().unwrap_or(0), *idx)
        })
        .map(|(_, (c, _))| *c)
}

/// 目标分片数：速率每满一个 [`WHALE_RPM_THRESHOLD`] 多一片，
/// 封顶 [`MAX_WHALE_SHARDS`]。速率低于阈值恒为 1（不分片）。
fn whale_target(rate_60s: usize) -> usize {
    (1 + rate_60s / WHALE_RPM_THRESHOLD).min(MAX_WHALE_SHARDS)
}

/// 分片内路由：rendezvous hashing。同一 shard_key（≈同一 session）恒定命中
/// 同一成员（保住该会话前缀缓存的局部性）；分片扩容时只有约 1/n 的 session
/// 需要换成员（取模路由几乎全体重排，每次扩容都会放大冷启）。
/// 未分片时直接返回主凭证，shard_key 不参与。
fn route_shard(shards: &[u64], primary: u64, shard_key: u64) -> u64 {
    if shards.is_empty() {
        return primary;
    }
    shards
        .iter()
        .copied()
        .max_by_key(|&s| mix64(shard_key ^ s.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .unwrap_or(primary)
}

/// SplitMix64 finalizer：足够雪崩性的廉价 64-bit mix，
/// 用于 rendezvous 打分（仅进程内路由，无跨进程稳定性要求）。
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无负载差异的候选列表（负载全 0），聚焦绑定/分片语义本身
    fn flat(ids: &[u64]) -> Vec<(u64, usize)> {
        ids.iter().map(|&c| (c, 0usize)).collect()
    }

    #[test]
    fn resolve_creates_new_binding() {
        let table = BindingTable::new();
        let picked = table.resolve(42, 0, &flat(&[1, 2, 3])).unwrap();
        assert!([1, 2, 3].contains(&picked));
        assert_eq!(table.resolve(42, 0, &flat(&[1, 2, 3])), Some(picked));
    }

    #[test]
    fn resolve_returns_none_when_no_credentials() {
        let table = BindingTable::new();
        assert_eq!(table.resolve(42, 0, &[]), None);
    }

    #[test]
    fn resolve_silently_rebinds_when_credential_unavailable() {
        let table = BindingTable::new();
        let first = table.resolve(42, 0, &flat(&[1])).unwrap();
        assert_eq!(first, 1);
        // 凭证 1 被移除，必须改绑到 2
        let second = table.resolve(42, 0, &flat(&[2])).unwrap();
        assert_eq!(second, 2);
    }

    #[test]
    fn least_bound_picks_empty_credential() {
        let table = BindingTable::new();
        // 把 3 个用户都绑到 1
        for uid in [10, 11, 12] {
            table.resolve(uid, 0, &flat(&[1])).unwrap();
        }
        // 新用户来，凭证 1 和 2 都可用（负载平局），应挑绑定少的 2
        let picked = table.resolve(99, 0, &flat(&[1, 2])).unwrap();
        assert_eq!(picked, 2);
    }

    #[test]
    fn placement_prefers_lower_external_load() {
        let table = BindingTable::new();
        // 凭证 1 外部负载 10（RPM 计数），凭证 2 空闲 → 新身份放置到 2，
        // 即使 1 排在候选首位且两者绑定数相同
        assert_eq!(table.resolve(42, 0, &[(1, 10), (2, 0)]), Some(2));
    }

    #[test]
    fn placement_load_beats_binding_count() {
        let table = BindingTable::new();
        // 凭证 2 已有 2 个 fresh 绑定但外部负载低；凭证 1 绑定少但负载高。
        // 负载是主信号 → 仍选 2（绑定数只做平局裁决）
        table.resolve(10, 0, &flat(&[2])).unwrap();
        table.resolve(11, 0, &flat(&[2])).unwrap();
        assert_eq!(table.resolve(42, 0, &[(1, 50), (2, 3)]), Some(2));
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
        assert_eq!(table.resolve(42, 0, &flat(&[1])), Some(1));
        let new_cred = table.rebind(42, 1, &flat(&[1, 2])).unwrap();
        assert_eq!(new_cred, 2);
        assert_eq!(table.resolve(42, 0, &flat(&[1, 2])), Some(2));
    }

    #[test]
    fn rebind_is_idempotent_when_user_already_moved() {
        // 回归：并发 report_error 可能让两个线程都判定需要 rebind。
        // 第二次 rebind 时用户已经迁出 `avoid`，必须 no-op，不然会被无意义
        // 挪到第三个凭证。
        let table = BindingTable::new();
        table.resolve(42, 0, &flat(&[1])).unwrap();
        let first = table.rebind(42, 1, &flat(&[1, 2, 3])).unwrap();
        assert_eq!(first, 2);
        // 第二次用相同 avoid=1 调用 rebind：用户已经在 2 上，不能再挪到 3
        let second = table.rebind(42, 1, &flat(&[1, 2, 3])).unwrap();
        assert_eq!(second, 2);
    }

    #[test]
    fn rebind_returns_none_when_no_alternative() {
        let table = BindingTable::new();
        table.resolve(42, 0, &flat(&[1])).unwrap();
        assert_eq!(table.rebind(42, 1, &flat(&[1])), None);
    }

    #[test]
    fn cold_binding_replaced_by_current_load_on_return() {
        let table = BindingTable::with_cold_idle(Duration::from_millis(10));
        // 42 绑到 1，随后冷透（空闲超过 cold_idle，上游缓存视为已过期）
        assert_eq!(table.resolve(42, 0, &flat(&[1])), Some(1));
        std::thread::sleep(Duration::from_millis(20));
        // 冷绑定不计数 → 1/2 平局，新身份 10 落到首位凭证 1（保持 fresh）
        assert_eq!(table.resolve(10, 0, &flat(&[1, 2])), Some(1));
        // 42 回流：绑定已冷，按当前负载重新放置 → 避开有 fresh 绑定的 1
        assert_eq!(table.resolve(42, 0, &flat(&[1, 2])), Some(2));
        // 重新放置后恢复粘性
        assert_eq!(table.resolve(42, 0, &flat(&[1, 2])), Some(2));
    }

    #[test]
    fn cold_bindings_do_not_count_toward_load() {
        let table = BindingTable::with_cold_idle(Duration::from_millis(10));
        table.resolve(42, 0, &flat(&[1])).unwrap();
        table.resolve(43, 0, &flat(&[1])).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // 42/43 已冷：不计入凭证 1 的占用。若仍计数会选空的 2；
        // 忽略后 1/2 平局，按 available 顺序取 1。
        assert_eq!(table.resolve(99, 0, &flat(&[1, 2])), Some(1));
    }

    #[test]
    fn fresh_binding_stays_sticky_within_cold_idle() {
        // 生产阈值下（75min）短暂间隔的请求必须保持粘性，不被再均衡误伤
        let table = BindingTable::new();
        assert_eq!(table.resolve(42, 0, &flat(&[1])), Some(1));
        // 凭证 2 完全空闲，但绑定未冷 → 仍粘住 1
        assert_eq!(table.resolve(42, 0, &flat(&[1, 2])), Some(1));
    }

    // ---- 鲸鱼分片 ----

    /// 让身份 `id` 在 60s 窗口内累积 `n` 次请求（模拟鲸鱼速率）
    fn pump(table: &BindingTable, id: u64, shard_key: u64, avail: &[(u64, usize)], n: usize) {
        for _ in 0..n {
            table.resolve(id, shard_key, avail).unwrap();
        }
    }

    #[test]
    fn normal_identity_never_shards_and_ignores_shard_key() {
        let table = BindingTable::new();
        let avail = flat(&[1, 2, 3]);
        let first = table.resolve(42, 0, &avail).unwrap();
        // 低于鲸鱼阈值的身份：不同 shard_key（不同 session）也恒定同一凭证
        for key in 1..(WHALE_RPM_THRESHOLD as u64 - 2) {
            assert_eq!(table.resolve(42, key, &avail), Some(first));
        }
    }

    #[test]
    fn whale_identity_shards_and_spreads_sessions() {
        let table = BindingTable::new();
        let avail = flat(&[1, 2, 3, 4]);
        // 速率拉满远超阈值：触发扩片（渐进，每次 resolve 最多一片）
        pump(&table, 42, 0, &avail, WHALE_RPM_THRESHOLD * 4);
        // 不同 session（shard_key）应摊到至少 2 个不同凭证
        let mut seen = std::collections::HashSet::new();
        for key in 0..32u64 {
            seen.insert(table.resolve(42, key, &avail).unwrap());
        }
        assert!(
            seen.len() >= 2,
            "鲸鱼分片后不同 session 应摊开，实际只落在 {:?}",
            seen
        );
        assert!(seen.len() <= MAX_WHALE_SHARDS, "分片数不得超过上限");
        // 仍然只有一条绑定（分片是绑定内部结构，不是多条绑定）
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn whale_same_session_routes_stably() {
        let table = BindingTable::new();
        let avail = flat(&[1, 2, 3]);
        pump(&table, 42, 0, &avail, WHALE_RPM_THRESHOLD * 4);
        // 同一 shard_key 必须恒定命中同一分片成员（会话缓存局部性）
        let pinned = table.resolve(42, 777, &avail).unwrap();
        for _ in 0..10 {
            assert_eq!(table.resolve(42, 777, &avail), Some(pinned));
        }
    }

    #[test]
    fn whale_shards_capped_by_available() {
        let table = BindingTable::new();
        let avail = flat(&[1, 2]);
        // 速率对应目标 4 片，但候选只有 2 个凭证 → 封顶 2
        pump(&table, 42, 0, &avail, WHALE_RPM_THRESHOLD * 8);
        let mut seen = std::collections::HashSet::new();
        for key in 0..32u64 {
            seen.insert(table.resolve(42, key, &avail).unwrap());
        }
        assert!(seen.len() <= 2);
    }

    #[test]
    fn whale_rebind_replaces_only_failing_member() {
        let table = BindingTable::new();
        // 候选只有 1/2：分片封顶 2 个成员
        let avail2 = flat(&[1, 2]);
        pump(&table, 42, 0, &avail2, WHALE_RPM_THRESHOLD * 4);
        // 找到命中两个不同成员的 shard_key，确认确实已分片
        let member_a = table.resolve(42, 0, &avail2).unwrap();
        let member_b = (1..256u64)
            .map(|key| table.resolve(42, key, &avail2).unwrap())
            .find(|&m| m != member_a)
            .expect("两个分片成员应各有 session 命中");

        // member_b 报错触发改绑，候选含新凭证 3：新成员排除现有成员 → 必选 3
        let replaced = table.rebind(42, member_b, &flat(&[1, 2, 3])).unwrap();
        assert_eq!(replaced, 3);
        // 出错成员已被摘除：后续路由（member_b 已冷却出候选）只落在存活成员上，
        // 且同一 shard_key 稳定命中同一成员
        let avail_after = flat(&[member_a, 3]);
        for key in 0..32u64 {
            let first = table.resolve(42, key, &avail_after).unwrap();
            assert!(first == member_a || first == 3);
            assert_eq!(table.resolve(42, key, &avail_after), Some(first));
        }
    }

    #[test]
    fn whale_resets_to_single_credential_after_cold() {
        let table = BindingTable::with_cold_idle(Duration::from_millis(10));
        let avail = flat(&[1, 2, 3]);
        pump(&table, 42, 0, &avail, WHALE_RPM_THRESHOLD * 4);
        std::thread::sleep(Duration::from_millis(20));
        // 冷透回流：鲸鱼状态重置，回到单凭证粘性（不同 shard_key 同凭证）
        let first = table.resolve(42, 1, &avail).unwrap();
        assert_eq!(table.resolve(42, 999, &avail), Some(first));
    }

    #[test]
    fn whale_survives_member_removal() {
        let table = BindingTable::new();
        let avail = flat(&[1, 2]);
        pump(&table, 42, 0, &avail, WHALE_RPM_THRESHOLD * 4);
        // 凭证 1 被禁用：路由必须全部收敛到存活的 2，不得 panic 或丢绑定
        for key in 0..16u64 {
            assert_eq!(table.resolve(42, key, &flat(&[2])), Some(2));
        }
    }

    // ---- sweep ----

    #[test]
    fn sweep_stale_removes_idle_bindings() {
        let table = BindingTable::new();
        table.resolve(42, 0, &flat(&[1])).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let removed = table.sweep_stale(Duration::from_millis(10));
        assert_eq!(removed, 1);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn sweep_stale_keeps_active_and_reports_count() {
        let table = BindingTable::new();
        table.resolve(1, 0, &flat(&[10])).unwrap();
        table.resolve(2, 0, &flat(&[10])).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // 用很大的 max_idle：两条都不算过期，移除 0 条
        let removed = table.sweep_stale(Duration::from_secs(3600));
        assert_eq!(removed, 0);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn sweep_stale_prunes_empty_cred_errors() {
        let table = BindingTable::new();
        // 制造一条 cred_errors 记录但不触发 rebind 阈值；
        // 验证 sweep 与 report_error 状态共存时不 panic、绑定计数正确。
        // （cred_errors 时间戳超 ERROR_WINDOW=60s 后才会被清空，单测不便等待，
        //  此处只覆盖 sweep 同时遍历两张表的路径。）
        table.report_error(99);
        std::thread::sleep(Duration::from_millis(20));
        table.sweep_stale(Duration::from_millis(10));
        assert_eq!(table.len(), 0);
    }
}

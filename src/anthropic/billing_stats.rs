//! 全局计费累计统计（带磁盘持久化）
//!
//! 进程维度累计每请求的实际成本、官方折算价与毛利（margin），供 admin 只读接口查询，
//! 并落盘到 `billing_stats.json`（与凭据缓存同目录），进程重启后累计值不丢失。
//!
//! 设计要点：
//! - 单例 `OnceLock<BillingStats>`，热路径累加只做无锁原子 `fetch_add(Relaxed)`，纳秒级、零分配。
//! - 金额以「微美元」（USD × 1e6）的 `i64` 累计，避免浮点反复相加的精度漂移；
//!   margin 可能为负（亏损请求），故用有符号整型。
//! - 仅在每请求收尾（流结束）记录一次，不进入逐 chunk 热循环。
//! - 落盘采用 debounce（默认 30s 一次），避免每请求同步写文件；同步小文件写在已限频下开销可忽略。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 落盘防抖间隔：两次写盘至少间隔该时长。
const SAVE_DEBOUNCE: Duration = Duration::from_secs(30);

/// 全局计费累计统计单例。
#[derive(Debug, Default)]
pub struct BillingStats {
    /// 累计请求数。
    requests: AtomicU64,
    /// 累计实际成本（微美元，USD × 1e6）。
    actual_cost_micro: AtomicI64,
    /// 累计官方折算价（微美元）。
    official_price_micro: AtomicI64,
    /// 累计毛利（微美元，可为负）。
    margin_micro: AtomicI64,
    /// 累计 stop_reason == "max_tokens" 的请求数（输出被截断/思考预算耗尽）。
    /// 与 requests 之比即「截断命中率」，用于判断上游默认输出上限是否偏低。
    max_tokens_truncated: AtomicU64,

    /// SQLite 连接（kiro.db；未配置时不持久化，仅进程内累计）。
    db: Mutex<Option<Connection>>,
    /// 上次落盘时间（用于 debounce）。
    last_save_at: Mutex<Option<Instant>>,
    /// 是否有未落盘的更新。
    dirty: AtomicBool,
}

/// 全局单例。
static GLOBAL: OnceLock<BillingStats> = OnceLock::new();

/// 获取全局计费统计单例。
pub fn global() -> &'static BillingStats {
    GLOBAL.get_or_init(BillingStats::default)
}

/// 初始化持久化：打开 kiro.db 并加载已有累计值。
///
/// 在进程启动时调用一次。`path` 为统一的 kiro.db 路径。
pub fn init_persistence(path: PathBuf) {
    global().init(path);
}

/// 打开 kiro.db 连接并建表（终身累计单行表）。
fn open_db(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS billing_totals (
            id                   INTEGER PRIMARY KEY CHECK (id = 1),
            requests             INTEGER NOT NULL DEFAULT 0,
            actual_micro         INTEGER NOT NULL DEFAULT 0,
            official_micro       INTEGER NOT NULL DEFAULT 0,
            margin_micro         INTEGER NOT NULL DEFAULT 0,
            max_tokens_truncated INTEGER NOT NULL DEFAULT 0
        ) STRICT;",
    )?;
    Ok(conn)
}

/// 将 USD 金额换算为微美元（四舍五入）。
fn to_micro(usd: f64) -> i64 {
    (usd * 1_000_000.0).round() as i64
}

/// 磁盘持久化的累计快照（微美元整型，避免浮点漂移）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedStats {
    requests: u64,
    actual_cost_micro: i64,
    official_price_micro: i64,
    margin_micro: i64,
    /// 旧缓存文件无此字段，缺失时默认 0。
    #[serde(default)]
    max_tokens_truncated: u64,
}

impl BillingStats {
    /// 打开 kiro.db、建表、加载已有累计值（表空时一次性从旧 billing_stats.json 迁移）。
    fn init(&self, path: PathBuf) {
        let conn = match open_db(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("计费统计库打开失败，仅进程内累计: {}", e);
                return;
            }
        };

        // 1) 优先从表加载。
        let loaded = conn
            .query_row(
                "SELECT requests, actual_micro, official_micro, margin_micro, max_tokens_truncated
                 FROM billing_totals WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .unwrap_or(None);

        if let Some((req, act, off, mar, trunc)) = loaded {
            self.requests.store(req as u64, Ordering::Relaxed);
            self.actual_cost_micro.store(act, Ordering::Relaxed);
            self.official_price_micro.store(off, Ordering::Relaxed);
            self.margin_micro.store(mar, Ordering::Relaxed);
            self.max_tokens_truncated.store(trunc as u64, Ordering::Relaxed);
            tracing::info!(requests = req, "已从 SQLite 加载计费累计统计");
        } else if let Some(p) = path
            .parent()
            .and_then(|d| std::fs::read_to_string(d.join("billing_stats.json")).ok())
            .and_then(|c| serde_json::from_str::<PersistedStats>(&c).ok())
        {
            // 2) 表空 + 旧 JSON 存在 → 一次性迁移。
            self.requests.store(p.requests, Ordering::Relaxed);
            self.actual_cost_micro.store(p.actual_cost_micro, Ordering::Relaxed);
            self.official_price_micro.store(p.official_price_micro, Ordering::Relaxed);
            self.margin_micro.store(p.margin_micro, Ordering::Relaxed);
            self.max_tokens_truncated.store(p.max_tokens_truncated, Ordering::Relaxed);
            tracing::info!(requests = p.requests, "已从 billing_stats.json 迁移计费累计入库");
        }

        *self.db.lock() = Some(conn);
        *self.last_save_at.lock() = Some(Instant::now());
        self.dirty.store(false, Ordering::Relaxed);
        // 落一行，确保 id=1 存在（迁移或全新启动时）。
        self.save();
    }

    /// 记录一次请求的计费结果。在每请求收尾路径调用一次。
    ///
    /// `truncated`：本次 stop_reason 是否为 `max_tokens`（输出被截断 / 思考预算耗尽）。
    pub fn record(&self, actual_usd: f64, official_usd: f64, margin_usd: f64, truncated: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.actual_cost_micro
            .fetch_add(to_micro(actual_usd), Ordering::Relaxed);
        self.official_price_micro
            .fetch_add(to_micro(official_usd), Ordering::Relaxed);
        self.margin_micro
            .fetch_add(to_micro(margin_usd), Ordering::Relaxed);
        if truncated {
            self.max_tokens_truncated.fetch_add(1, Ordering::Relaxed);
        }
        self.save_debounced();
    }

    /// 标记脏并按 debounce 策略决定是否立即落盘。
    fn save_debounced(&self) {
        self.dirty.store(true, Ordering::Relaxed);

        // 未配置持久化则跳过（仅进程内累计）。
        if self.db.lock().is_none() {
            return;
        }

        let should_flush = match *self.last_save_at.lock() {
            Some(last) => last.elapsed() >= SAVE_DEBOUNCE,
            None => true,
        };
        if should_flush {
            self.save();
        }
    }

    /// 立即落盘当前累计值（UPSERT 单行 id=1）。
    fn save(&self) {
        let guard = self.db.lock();
        let conn = match guard.as_ref() {
            Some(c) => c,
            None => return,
        };

        let r = conn.execute(
            "INSERT INTO billing_totals
                (id, requests, actual_micro, official_micro, margin_micro, max_tokens_truncated)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                requests = excluded.requests,
                actual_micro = excluded.actual_micro,
                official_micro = excluded.official_micro,
                margin_micro = excluded.margin_micro,
                max_tokens_truncated = excluded.max_tokens_truncated",
            rusqlite::params![
                self.requests.load(Ordering::Relaxed) as i64,
                self.actual_cost_micro.load(Ordering::Relaxed),
                self.official_price_micro.load(Ordering::Relaxed),
                self.margin_micro.load(Ordering::Relaxed),
                self.max_tokens_truncated.load(Ordering::Relaxed) as i64,
            ],
        );
        match r {
            Ok(_) => {
                *self.last_save_at.lock() = Some(Instant::now());
                self.dirty.store(false, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!("保存计费累计失败: {}", e),
        }
    }

    /// 读取当前累计快照（用于查询接口）。
    pub fn snapshot(&self) -> BillingStatsSnapshot {
        let actual_micro = self.actual_cost_micro.load(Ordering::Relaxed);
        let official_micro = self.official_price_micro.load(Ordering::Relaxed);
        let margin_micro = self.margin_micro.load(Ordering::Relaxed);
        let requests = self.requests.load(Ordering::Relaxed);
        let truncated = self.max_tokens_truncated.load(Ordering::Relaxed);
        BillingStatsSnapshot {
            requests,
            actual_cost_usd: actual_micro as f64 / 1_000_000.0,
            official_price_usd: official_micro as f64 / 1_000_000.0,
            margin_usd: margin_micro as f64 / 1_000_000.0,
            max_tokens_truncated: truncated,
            max_tokens_truncated_rate: if requests > 0 {
                truncated as f64 / requests as f64
            } else {
                0.0
            },
        }
    }
}

/// 计费累计快照（对外 JSON 序列化）。
#[derive(Debug, Clone, Serialize)]
pub struct BillingStatsSnapshot {
    /// 累计请求数。
    pub requests: u64,
    /// 累计实际成本（USD）。
    pub actual_cost_usd: f64,
    /// 累计官方折算价（USD）。
    pub official_price_usd: f64,
    /// 累计毛利（USD，可为负）。
    pub margin_usd: f64,
    /// 累计 stop_reason == "max_tokens" 的请求数（输出被截断）。
    pub max_tokens_truncated: u64,
    /// 截断命中率 = max_tokens_truncated / requests（无请求时为 0）。
    pub max_tokens_truncated_rate: f64,
}

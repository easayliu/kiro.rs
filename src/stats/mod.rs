//! 请求级时序统计（SQLite 持久化），供 admin 出成本/用量曲线与分析。
//!
//! 与 `anthropic::billing_stats`（进程维度的「累计总数」）互补：这里**按条**记录每个
//! 请求的成本/token/延迟/截断，落盘到 SQLite，admin 用聚合 SQL 出按时间分桶的曲线，
//! 并能按 model / credential 切片。
//!
//! 设计要点（与项目「不阻塞 async 执行器」的取向一致）：
//! - 热路径只做一次无阻塞 `try_send` 到有界 channel；channel 满直接丢弃，绝不阻塞请求收尾。
//! - 后台**单写线程**持有唯一写连接，攒批 + 定时 flush（一个事务一批），SQLite 开 WAL。
//! - admin 查询每次开一个独立连接、在 `spawn_blocking` 里跑聚合 SQL（WAL 允许并发读）。
//! - 仅保留 N 天：启动时与每 6h 定时 `DELETE` 过期行。
//!
//! 未启用 `stats` feature 时，所有公开函数退化为 no-op / 返回空，调用点无需 `cfg`。

use serde::Serialize;
use std::path::PathBuf;

/// 单条请求的统计样本。`ts` 由 [`record`] 在入队时统一打点，调用方无需填写。
#[derive(Clone, Debug, Default)]
pub struct RequestStat {
    /// Unix 秒时间戳（由 record 填充）。
    pub ts: i64,
    pub model: String,
    pub credential_id: i64,
    /// 实际成本（微美元，USD × 1e6）。
    pub actual_micro: i64,
    /// 官方折算价（微美元）。
    pub official_micro: i64,
    /// 毛利（微美元，可为负）。
    pub margin_micro: i64,
    /// 未缓存输入 token。
    pub input_tokens: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub output_tokens: i64,
    /// 首字耗时（毫秒）；非流式记 0。
    pub ttft_ms: i64,
    /// 总耗时（毫秒）。
    pub elapsed_ms: i64,
    /// 请求结果状态码：`0` = 成功（正常完成）；
    /// 非 0 = 失败请求（上游 API 错误），存上游 HTTP status（如 400/401/429/500），
    /// 无 HTTP 响应的内部错误用映射码（无可用凭据 503、其它 502）。
    /// 失败行的 token/成本/延迟均为 0，仅用于错误率统计，不污染成功侧聚合。
    pub status_code: i64,
}

/// 曲线分组维度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    None,
    Model,
    Credential,
}

impl GroupBy {
    pub fn parse(s: &str) -> Self {
        match s {
            "model" => GroupBy::Model,
            "credential" => GroupBy::Credential,
            _ => GroupBy::None,
        }
    }
}

/// 一个时间桶的聚合结果。
#[derive(Debug, Serialize)]
pub struct TimeBucket {
    /// 桶起始时间（Unix 秒）。
    pub bucket: i64,
    /// 分组键（分组时存在）：model 名或 credential id 字符串。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub requests: i64,
    /// 失败请求数（status_code != 0，上游 API 错误）。
    pub failures: i64,
    pub actual_usd: f64,
    pub official_usd: f64,
    pub margin_usd: f64,
    pub input_tokens: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub output_tokens: i64,
    pub avg_ttft_ms: i64,
    pub avg_elapsed_ms: i64,
}

/// 一个分组（或全量）的汇总。
#[derive(Debug, Serialize, Default)]
pub struct StatGroup {
    /// 分组键：model 名 / credential id；全量汇总时为空串。
    pub key: String,
    pub requests: i64,
    /// 失败请求数（status_code != 0，上游 API 错误）。
    pub failures: i64,
    pub actual_usd: f64,
    pub official_usd: f64,
    pub margin_usd: f64,
    pub input_tokens: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub output_tokens: i64,
    pub avg_ttft_ms: i64,
    pub avg_elapsed_ms: i64,
}

/// 区间汇总：全量 + 按模型 + 按凭据。
#[derive(Debug, Serialize, Default)]
pub struct StatsSummary {
    pub total: StatGroup,
    pub by_model: Vec<StatGroup>,
    pub by_credential: Vec<StatGroup>,
}

fn micro_to_usd(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

// ===================== feature = "stats" 实现 =====================

#[cfg(feature = "stats")]
mod imp {
    use super::*;
    use parking_lot::Once;
    use rusqlite::Connection;
    use std::sync::OnceLock;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
    use std::time::{Duration, Instant};

    /// 有界队列容量；满则丢弃，保证热路径永不阻塞。
    const CHANNEL_CAP: usize = 4096;
    /// 单事务最多攒多少条。
    const BATCH_MAX: usize = 256;
    /// 攒批最长等待。
    const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
    /// 过期清理间隔。
    const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 3600);

    static SENDER: OnceLock<SyncSender<RequestStat>> = OnceLock::new();
    static DB_PATH: OnceLock<PathBuf> = OnceLock::new();
    static INIT: Once = Once::new();

    fn now_ts() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// 打开写连接并设置 PRAGMA（WAL + NORMAL 同步，兼顾安全与吞吐）。
    fn open_conn(path: &PathBuf) -> rusqlite::Result<Connection> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            // STRICT：严格类型校验（SQLite 3.37+），杜绝类型误塞。
            // id 用 INTEGER PRIMARY KEY（自增 rowid 别名）即可——不用 AUTOINCREMENT，
            // 后者只为「id 永不复用 + 严格单调」服务，带额外开销，这里不需要。
            "CREATE TABLE IF NOT EXISTS request_stats (
                id             INTEGER PRIMARY KEY,
                ts             INTEGER NOT NULL,
                model          TEXT    NOT NULL,
                credential_id  INTEGER NOT NULL,
                actual_micro   INTEGER NOT NULL,
                official_micro INTEGER NOT NULL,
                margin_micro   INTEGER NOT NULL,
                input_tokens   INTEGER NOT NULL,
                cache_read     INTEGER NOT NULL,
                cache_creation INTEGER NOT NULL,
                output_tokens  INTEGER NOT NULL,
                ttft_ms        INTEGER NOT NULL,
                elapsed_ms     INTEGER NOT NULL,
                -- 0=成功；非0=失败请求(上游 API 错误)的状态码。失败行其余指标均为 0。
                status_code    INTEGER NOT NULL DEFAULT 0
            ) STRICT;
            -- 时序分桶 / 总量汇总 / 过期清理：均以 ts 区间为主过滤条件。
            CREATE INDEX IF NOT EXISTS idx_request_stats_ts ON request_stats(ts);
            -- 按模型汇总（WHERE ts 区间 GROUP BY model）：(model, ts) 让区间过滤后
            -- 直接按 model 有序分组，省去 GROUP BY 的额外排序。
            CREATE INDEX IF NOT EXISTS idx_request_stats_model_ts ON request_stats(model, ts);
            -- 按凭据汇总同理。
            CREATE INDEX IF NOT EXISTS idx_request_stats_cred_ts ON request_stats(credential_id, ts);",
        )?;
        // 迁移：老库（status_code 列出现前建的表）补列，默认 0=成功。
        // STRICT 表允许 ADD COLUMN ... NOT NULL DEFAULT。
        let has_status = conn
            .prepare("SELECT 1 FROM pragma_table_info('request_stats') WHERE name = 'status_code'")?
            .exists([])?;
        if !has_status {
            conn.execute_batch(
                "ALTER TABLE request_stats ADD COLUMN status_code INTEGER NOT NULL DEFAULT 0;",
            )?;
            tracing::info!("统计库已迁移：request_stats 新增 status_code 列");
        }
        // 迁移：删除已废弃的 truncated 列（kiro 不支持 max_tokens 截断，指标无意义）。
        let has_truncated = conn
            .prepare("SELECT 1 FROM pragma_table_info('request_stats') WHERE name = 'truncated'")?
            .exists([])?;
        if has_truncated {
            conn.execute_batch("ALTER TABLE request_stats DROP COLUMN truncated;")?;
            tracing::info!("统计库已迁移：request_stats 删除废弃的 truncated 列");
        }
        Ok(())
    }

    pub fn init(path: PathBuf, retention_days: u32) {
        INIT.call_once(|| {
            let conn = match open_conn(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("统计库打开失败，时序统计关闭: {}", e);
                    return;
                }
            };
            if let Err(e) = init_schema(&conn) {
                tracing::error!("统计库建表失败，时序统计关闭: {}", e);
                return;
            }
            let _ = DB_PATH.set(path.clone());
            let (tx, rx) = sync_channel::<RequestStat>(CHANNEL_CAP);
            if SENDER.set(tx).is_err() {
                return;
            }
            let retention = retention_days.max(1);
            let builder = std::thread::Builder::new().name("stats-writer".into());
            if let Err(e) = builder.spawn(move || writer_loop(conn, rx, retention)) {
                tracing::error!("统计写线程启动失败: {}", e);
            } else {
                tracing::info!("请求时序统计已启用: {:?}（保留 {} 天）", path, retention);
            }
        });
    }

    pub fn record(mut stat: RequestStat) {
        if let Some(tx) = SENDER.get() {
            stat.ts = now_ts();
            // 队列满时直接丢弃：宁可少几条统计，也不阻塞请求收尾。
            let _ = tx.try_send(stat);
        }
    }

    fn writer_loop(mut conn: Connection, rx: Receiver<RequestStat>, retention_days: u32) {
        prune(&conn, retention_days);
        let mut last_prune = Instant::now();
        loop {
            // 阻塞等待第一条；所有 sender 释放（进程退出）则结束。
            let first = match rx.recv() {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut batch = vec![first];
            let deadline = Instant::now() + FLUSH_INTERVAL;
            while batch.len() < BATCH_MAX {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(s) => batch.push(s),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        flush_batch(&mut conn, &batch);
                        return;
                    }
                }
            }
            flush_batch(&mut conn, &batch);
            if last_prune.elapsed() >= PRUNE_INTERVAL {
                prune(&conn, retention_days);
                last_prune = Instant::now();
            }
        }
    }

    fn flush_batch(conn: &mut Connection, batch: &[RequestStat]) {
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("统计写事务开启失败，丢弃 {} 条: {}", batch.len(), e);
                return;
            }
        };
        {
            let mut stmt = match tx.prepare_cached(
                "INSERT INTO request_stats
                    (ts, model, credential_id, actual_micro, official_micro, margin_micro,
                     input_tokens, cache_read, cache_creation, output_tokens,
                     ttft_ms, elapsed_ms, status_code)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("统计写 prepare 失败: {}", e);
                    return;
                }
            };
            for s in batch {
                let _ = stmt.execute(rusqlite::params![
                    s.ts,
                    s.model,
                    s.credential_id,
                    s.actual_micro,
                    s.official_micro,
                    s.margin_micro,
                    s.input_tokens,
                    s.cache_read,
                    s.cache_creation,
                    s.output_tokens,
                    s.ttft_ms,
                    s.elapsed_ms,
                    s.status_code,
                ]);
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("统计写提交失败: {}", e);
        }
    }

    fn prune(conn: &Connection, retention_days: u32) {
        let cutoff = now_ts() - (retention_days as i64) * 86_400;
        if let Err(e) = conn.execute("DELETE FROM request_stats WHERE ts < ?1", [cutoff]) {
            tracing::warn!("统计过期清理失败: {}", e);
        }
        // 刷新查询计划器统计（增量、低开销），让复合索引在数据增长后被正确选中。
        // 跟随 prune 的节奏（启动 + 每 6h）即可，无需单独定时。
        if let Err(e) = conn.execute_batch("PRAGMA optimize;") {
            tracing::warn!("统计 optimize 失败: {}", e);
        }
    }

    /// 分组维度对应的 SELECT 列与 GROUP BY 片段（枚举控制，无注入风险）。
    fn group_sql(group_by: GroupBy) -> (&'static str, &'static str) {
        match group_by {
            GroupBy::None => ("", ""),
            GroupBy::Model => ("model AS grp,", ", grp"),
            GroupBy::Credential => ("CAST(credential_id AS TEXT) AS grp,", ", grp"),
        }
    }

    /// 构建 `WHERE` 子句与参数：时间区间 + 可选 model/credential IN 过滤。
    /// 用 `?` 位置参数 + `params_from_iter`，IN 列表按选中项动态展开，无注入风险。
    fn build_filter(
        from_ts: i64,
        to_ts: i64,
        models: &[String],
        credentials: &[i64],
    ) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut sql = String::from("ts >= ? AND ts < ?");
        let mut params: Vec<Value> = vec![Value::Integer(from_ts), Value::Integer(to_ts)];
        if !models.is_empty() {
            let ph = std::iter::repeat("?").take(models.len()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND model IN ({ph})"));
            params.extend(models.iter().map(|m| Value::Text(m.clone())));
        }
        if !credentials.is_empty() {
            let ph = std::iter::repeat("?").take(credentials.len()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND credential_id IN ({ph})"));
            params.extend(credentials.iter().map(|c| Value::Integer(*c)));
        }
        (sql, params)
    }

    pub fn query_timeseries(
        from_ts: i64,
        to_ts: i64,
        bucket_secs: i64,
        group_by: GroupBy,
        models: &[String],
        credentials: &[i64],
    ) -> Vec<TimeBucket> {
        let path = match DB_PATH.get() {
            Some(p) => p.clone(),
            None => return Vec::new(),
        };
        let bucket = bucket_secs.max(1);
        let (grp_sel, grp_by) = group_sql(group_by);
        let has_group = group_by != GroupBy::None;
        let (where_sql, params) = build_filter(from_ts, to_ts, models, credentials);
        let sql = format!(
            // requests 只数成功（status_code=0）；failures 数失败（!=0）。
            // 成本/token/truncated 失败行均为 0，SUM 不受影响；延迟 AVG 用 CASE
            // 过滤成功行（CASE 无 ELSE → 失败行为 NULL，AVG 自动忽略）。
            "SELECT ({bucket_div}) AS bucket, {grp_sel}
                SUM(CASE WHEN status_code = 0 THEN 1 ELSE 0 END) AS requests,
                SUM(CASE WHEN status_code <> 0 THEN 1 ELSE 0 END) AS failures,
                SUM(actual_micro), SUM(official_micro), SUM(margin_micro),
                SUM(input_tokens), SUM(cache_read), SUM(cache_creation), SUM(output_tokens),
                CAST(COALESCE(AVG(CASE WHEN status_code = 0 THEN ttft_ms END),0) AS INTEGER),
                CAST(COALESCE(AVG(CASE WHEN status_code = 0 THEN elapsed_ms END),0) AS INTEGER)
             FROM request_stats
             WHERE {where_sql}
             GROUP BY bucket{grp_by}
             ORDER BY bucket ASC",
            bucket_div = format!("(ts / {bucket}) * {bucket}"),
        );

        let conn = match open_conn(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("统计查询连接失败: {}", e);
                return Vec::new();
            }
        };
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("统计查询 prepare 失败: {}", e);
                return Vec::new();
            }
        };
        // 分组时第 1 列后多一列 grp，后续列整体右移。
        let off = if has_group { 1 } else { 0 };
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let group = if has_group {
                Some(row.get::<_, String>(1)?)
            } else {
                None
            };
            Ok(TimeBucket {
                bucket: row.get(0)?,
                group,
                requests: row.get(1 + off)?,
                failures: row.get(2 + off)?,
                actual_usd: micro_to_usd(row.get(3 + off)?),
                official_usd: micro_to_usd(row.get(4 + off)?),
                margin_usd: micro_to_usd(row.get(5 + off)?),
                input_tokens: row.get(6 + off)?,
                cache_read: row.get(7 + off)?,
                cache_creation: row.get(8 + off)?,
                output_tokens: row.get(9 + off)?,
                avg_ttft_ms: row.get(10 + off)?,
                avg_elapsed_ms: row.get(11 + off)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(e) => {
                tracing::warn!("统计查询执行失败: {}", e);
                Vec::new()
            }
        }
    }

    /// 汇总聚合 SELECT 主体（不含分组键列），列序与 [`map_group`] 对应。
    const AGG_COLS: &str = "SUM(CASE WHEN status_code = 0 THEN 1 ELSE 0 END),
        SUM(CASE WHEN status_code <> 0 THEN 1 ELSE 0 END),
        SUM(actual_micro), SUM(official_micro), SUM(margin_micro),
        SUM(input_tokens), SUM(cache_read), SUM(cache_creation), SUM(output_tokens),
        CAST(COALESCE(AVG(CASE WHEN status_code = 0 THEN ttft_ms END),0) AS INTEGER),
        CAST(COALESCE(AVG(CASE WHEN status_code = 0 THEN elapsed_ms END),0) AS INTEGER)";

    /// 把聚合行（从 `base` 列开始的 11 列）映射成 StatGroup（key 由调用方填）。
    fn map_group(row: &rusqlite::Row, base: usize, key: String) -> rusqlite::Result<StatGroup> {
        Ok(StatGroup {
            key,
            requests: row.get(base)?,
            failures: row.get(base + 1)?,
            actual_usd: micro_to_usd(row.get(base + 2)?),
            official_usd: micro_to_usd(row.get(base + 3)?),
            margin_usd: micro_to_usd(row.get(base + 4)?),
            input_tokens: row.get(base + 5)?,
            cache_read: row.get(base + 6)?,
            cache_creation: row.get(base + 7)?,
            output_tokens: row.get(base + 8)?,
            avg_ttft_ms: row.get(base + 9)?,
            avg_elapsed_ms: row.get(base + 10)?,
        })
    }

    pub fn query_summary(
        from_ts: i64,
        to_ts: i64,
        models: &[String],
        credentials: &[i64],
    ) -> StatsSummary {
        let path = match DB_PATH.get() {
            Some(p) => p.clone(),
            None => return StatsSummary::default(),
        };
        let conn = match open_conn(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("统计汇总连接失败: {}", e);
                return StatsSummary::default();
            }
        };

        let (where_sql, params) = build_filter(from_ts, to_ts, models, credentials);

        // 全量。
        let total = conn
            .query_row(
                &format!("SELECT {AGG_COLS} FROM request_stats WHERE {where_sql}"),
                rusqlite::params_from_iter(params.iter()),
                |row| map_group(row, 0, String::new()),
            )
            .unwrap_or_default();

        let grouped = |col: &str| -> Vec<StatGroup> {
            let sql = format!(
                "SELECT {col} AS k, {AGG_COLS}
                 FROM request_stats WHERE {where_sql}
                 GROUP BY k ORDER BY SUM(official_micro) DESC",
            );
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                let key: String = row.get(0)?;
                map_group(row, 1, key)
            });
            match rows {
                Ok(it) => it.filter_map(Result::ok).collect(),
                Err(_) => Vec::new(),
            }
        };

        StatsSummary {
            total,
            by_model: grouped("model"),
            by_credential: grouped("CAST(credential_id AS TEXT)"),
        }
    }
}

// ===================== 公开 API（feature 开/关统一入口） =====================

/// 初始化时序统计：打开/建表 SQLite、启动后台写线程。进程启动调用一次。
pub fn init(path: PathBuf, retention_days: u32) {
    #[cfg(feature = "stats")]
    imp::init(path, retention_days);
    #[cfg(not(feature = "stats"))]
    {
        let _ = (path, retention_days);
    }
}

/// 记录一条请求统计。热路径调用，非阻塞；未启用 feature 时为 no-op。
pub fn record(stat: RequestStat) {
    #[cfg(feature = "stats")]
    imp::record(stat);
    #[cfg(not(feature = "stats"))]
    {
        let _ = stat;
    }
}

/// 查询时序曲线（按 `bucket_secs` 分桶，可按 model / credential 分组）。
/// `models` / `credentials` 为可选过滤（空=不过滤），二者可叠加。
pub async fn query_timeseries(
    from_ts: i64,
    to_ts: i64,
    bucket_secs: i64,
    group_by: GroupBy,
    models: Vec<String>,
    credentials: Vec<i64>,
) -> Vec<TimeBucket> {
    #[cfg(feature = "stats")]
    {
        tokio::task::spawn_blocking(move || {
            imp::query_timeseries(from_ts, to_ts, bucket_secs, group_by, &models, &credentials)
        })
        .await
        .unwrap_or_default()
    }
    #[cfg(not(feature = "stats"))]
    {
        let _ = (from_ts, to_ts, bucket_secs, group_by, models, credentials);
        Vec::new()
    }
}

/// 查询区间汇总（全量 + 按模型 + 按凭据）。`models` / `credentials` 为可选过滤，可叠加。
pub async fn query_summary(
    from_ts: i64,
    to_ts: i64,
    models: Vec<String>,
    credentials: Vec<i64>,
) -> StatsSummary {
    #[cfg(feature = "stats")]
    {
        tokio::task::spawn_blocking(move || imp::query_summary(from_ts, to_ts, &models, &credentials))
            .await
            .unwrap_or_default()
    }
    #[cfg(not(feature = "stats"))]
    {
        let _ = (from_ts, to_ts, models, credentials);
        StatsSummary::default()
    }
}

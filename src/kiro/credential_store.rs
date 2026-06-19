//! 凭据的 SQLite 持久化层（替代 `credentials.json` 的原地全量重写）。
//!
//! 设计动机：几千个凭据 + 1h 轮换的 refresh token，原地重写整个 JSON 文件既有写放大、
//! 又有「写到一半被 kill → 文件损坏 → 凭据全失效」的风险。改为 SQLite：
//! - **完全规范化**表（每字段一列）+ `STRICT` 严格类型 + `CHECK`/`UNIQUE` 约束防脏数据
//! - token 轮换走**单行 `UPDATE`**（[`upsert`](CredentialStore::upsert)），不再全量重写
//! - 写入是事务 + WAL，原子且持久；半写不会损坏库
//!
//! 索引：仅主键（所有访问按 id），外加 `(priority,id)`、`group_name` 两个**低频**列索引
//! （DB 侧 `ORDER BY priority,id` 载入；故意不索引高频变动的 `expires_at`）。

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, Row, params};

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::ClientMode;

/// 迁移报告：从 `credentials.json` 导入时的清洗统计。
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// 成功导入的条数。
    pub imported: usize,
    /// 因 refresh_token 重复被跳过的条数（保留 expires_at 最新的一条）。
    pub deduped: usize,
    /// 因既无 refresh_token 又无 kiro_api_key（无法鉴权的死凭据）被跳过的条数。
    pub skipped_dead: usize,
}

/// 凭据 SQLite 存储。内部单连接 + `Mutex` 串行化（写少，临界区为纯同步小 IO）。
pub struct CredentialStore {
    conn: Mutex<Connection>,
}

impl CredentialStore {
    /// 打开（或新建）凭据库并初始化 schema。
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开凭据库失败: {:?}", path))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS credentials (
                id                 INTEGER PRIMARY KEY,
                access_token       TEXT,
                refresh_token      TEXT,
                profile_arn        TEXT,
                expires_at         INTEGER,
                auth_method        TEXT,
                kiro_api_key       TEXT,
                client_id          TEXT,
                client_secret      TEXT,
                region             TEXT,
                auth_region        TEXT,
                api_region         TEXT,
                machine_id         TEXT,
                client_mode        TEXT,
                proxy_url          TEXT,
                proxy_username     TEXT,
                proxy_password     TEXT,
                group_name         TEXT,
                email              TEXT,
                subscription_title TEXT,
                priority           INTEGER NOT NULL DEFAULT 0,
                disabled           INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
                rpm_limit          INTEGER CHECK (rpm_limit IS NULL OR rpm_limit >= 0),
                concurrency_limit  INTEGER CHECK (concurrency_limit IS NULL OR concurrency_limit >= 0),
                overage            INTEGER CHECK (overage IN (0,1)),
                created_at         INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at         INTEGER NOT NULL DEFAULT (unixepoch()),
                CHECK (refresh_token IS NOT NULL OR kiro_api_key IS NOT NULL)
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_credentials_priority ON credentials(priority, id);
            CREATE INDEX IF NOT EXISTS idx_credentials_group    ON credentials(group_name);
            CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
                ON credentials(refresh_token) WHERE refresh_token IS NOT NULL;

            CREATE TRIGGER IF NOT EXISTS trg_credentials_touch
            AFTER UPDATE ON credentials FOR EACH ROW
            WHEN NEW.updated_at = OLD.updated_at
            BEGIN
                UPDATE credentials SET updated_at = unixepoch() WHERE id = NEW.id;
            END;

            -- 每凭据使用统计（替代 kiro_stats.json，与凭据 1:1）。
            CREATE TABLE IF NOT EXISTS credential_stats (
                credential_id INTEGER PRIMARY KEY,
                success_count INTEGER NOT NULL DEFAULT 0,
                last_used_at  INTEGER
            ) STRICT;",
        )
        .context("初始化凭据库 schema 失败")?;
        Ok(())
    }

    /// 库中是否没有任何凭据（用于判断是否需要从 JSON 迁移）。
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    /// 载入全部凭据，按 (priority, id) 升序（DB 侧排序，命中 `idx_credentials_priority`）。
    pub fn load_all(&self) -> anyhow::Result<Vec<KiroCredentials>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM credentials ORDER BY priority ASC, id ASC"
        ))?;
        let rows = stmt.query_map([], row_to_cred)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// upsert 单条凭据（按 id 冲突更新）。token 轮换 / 单字段改动走这里，只动一行。
    pub fn upsert(&self, cred: &KiroCredentials) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        upsert_with(&conn, cred)
    }

    /// 全量同步：把内存中的凭据集落库（upsert 全部 + 删除集合外的行），单事务原子。
    /// 供批量 admin 操作 / 删除后回写（频率低，几千行事务亦在毫秒级）。
    pub fn sync_all(&self, creds: &[KiroCredentials]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            // 删除不在当前集合里的行（反映 admin 删除）。
            let keep: Vec<i64> = creds.iter().filter_map(|c| c.id.map(|i| i as i64)).collect();
            let placeholders = keep.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = if keep.is_empty() {
                "DELETE FROM credentials".to_string()
            } else {
                format!("DELETE FROM credentials WHERE id NOT IN ({placeholders})")
            };
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                keep.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            tx.execute(&sql, params_ref.as_slice())?;
            for cred in creds {
                upsert_with(&tx, cred)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `credential_stats` 表是否为空（用于判断是否需要从 kiro_stats.json 迁移）。
    pub fn credential_stats_is_empty(&self) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM credential_stats", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    /// 载入全部凭据使用统计：`(id, success_count, last_used_at_rfc3339)`。
    pub fn load_credential_stats(&self) -> anyhow::Result<Vec<(u64, u64, Option<String>)>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT credential_id, success_count, last_used_at FROM credential_stats")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let sc: i64 = row.get(1)?;
            let lu: Option<i64> = row.get(2)?;
            Ok((id as u64, sc as u64, unix_to_rfc3339(lu)))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 全量写入凭据使用统计（upsert，单事务）。`last_used_at` 为 RFC3339，落库转 unix 秒。
    pub fn save_credential_stats(
        &self,
        stats: &[(u64, u64, Option<String>)],
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO credential_stats (credential_id, success_count, last_used_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(credential_id) DO UPDATE SET
                    success_count = excluded.success_count,
                    last_used_at  = excluded.last_used_at",
            )?;
            for (id, sc, lu) in stats {
                let lu_unix = lu
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.timestamp());
                stmt.execute(params![*id as i64, *sc as i64, lu_unix])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 从 JSON 载入的凭据迁移入库：**去重**（同 refresh_token 保留 expires_at 最新）、
    /// **跳过死号**（无 refresh_token 且无 kiro_api_key）、为缺失 id 的补号，单事务导入。
    pub fn migrate_from_json(
        &self,
        creds: Vec<KiroCredentials>,
    ) -> anyhow::Result<MigrationReport> {
        let mut report = MigrationReport::default();

        // 1) 跳过死号
        let mut alive: Vec<KiroCredentials> = Vec::with_capacity(creds.len());
        for c in creds {
            let has_refresh = c.refresh_token.as_deref().is_some_and(|s| !s.is_empty());
            let has_api_key = c.kiro_api_key.as_deref().is_some_and(|s| !s.is_empty());
            if !has_refresh && !has_api_key {
                report.skipped_dead += 1;
                continue;
            }
            alive.push(c);
        }

        // 2) 按 refresh_token 去重：保留 expires_at 最新的一条（None 视为最旧）。
        //    无 refresh_token 的（纯 api_key）不参与去重，全部保留。
        use std::collections::HashMap;
        let mut best_by_token: HashMap<String, usize> = HashMap::new();
        let mut keep_flags = vec![true; alive.len()];
        for (i, c) in alive.iter().enumerate() {
            let token = match c.refresh_token.as_deref() {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => continue,
            };
            match best_by_token.get(&token).copied() {
                None => {
                    best_by_token.insert(token, i);
                }
                Some(prev) => {
                    // 比较 expires_at，保留更新的；淘汰另一条。
                    let keep_new = expires_unix(&alive[i]) >= expires_unix(&alive[prev]);
                    if keep_new {
                        keep_flags[prev] = false;
                        best_by_token.insert(token, i);
                    } else {
                        keep_flags[i] = false;
                    }
                    report.deduped += 1;
                }
            }
        }

        // 3) 为缺失 id 的补号（max 现有 + 递增）。
        let mut next_id = alive
            .iter()
            .filter_map(|c| c.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        // 4) 单事务插入。
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            for (i, mut c) in alive.into_iter().enumerate() {
                if !keep_flags[i] {
                    continue;
                }
                if c.id.is_none() {
                    c.id = Some(next_id);
                    next_id += 1;
                }
                upsert_with(&tx, &c)?;
                report.imported += 1;
            }
        }
        tx.commit()?;
        Ok(report)
    }
}

/// 列清单（与 [`row_to_cred`] 的取值顺序一一对应）。
const COLS: &str = "id, access_token, refresh_token, profile_arn, expires_at, auth_method,
    kiro_api_key, client_id, client_secret, region, auth_region, api_region, machine_id,
    client_mode, proxy_url, proxy_username, proxy_password, group_name, email,
    subscription_title, priority, disabled, rpm_limit, concurrency_limit, overage";

/// 把一行映射回 `KiroCredentials`（列序同 [`COLS`]）。
fn row_to_cred(row: &Row) -> rusqlite::Result<KiroCredentials> {
    let expires_unix: Option<i64> = row.get(4)?;
    let client_mode_str: Option<String> = row.get(13)?;
    let disabled_i: i64 = row.get(21)?;
    let rpm: Option<i64> = row.get(22)?;
    let conc: Option<i64> = row.get(23)?;
    let overage_i: Option<i64> = row.get(24)?;

    Ok(KiroCredentials {
        id: row.get::<_, i64>(0).map(|v| v as u64).ok(),
        access_token: row.get(1)?,
        refresh_token: row.get(2)?,
        profile_arn: row.get(3)?,
        expires_at: unix_to_rfc3339(expires_unix),
        auth_method: row.get(5)?,
        kiro_api_key: row.get(6)?,
        client_id: row.get(7)?,
        client_secret: row.get(8)?,
        region: row.get(9)?,
        auth_region: row.get(10)?,
        api_region: row.get(11)?,
        machine_id: row.get(12)?,
        client_mode: client_mode_from_str(client_mode_str),
        proxy_url: row.get(14)?,
        proxy_username: row.get(15)?,
        proxy_password: row.get(16)?,
        group: row.get(17)?,
        email: row.get(18)?,
        subscription_title: row.get(19)?,
        priority: row.get::<_, i64>(20)? as u32,
        disabled: disabled_i != 0,
        rpm_limit: rpm.map(|v| v as u32),
        concurrency_limit: conc.map(|v| v as u32),
        overage: overage_i.map(|v| v != 0),
    })
}

/// upsert 实现（连接 / 事务通用，`Connection` 与 `Transaction` 都 deref 到可执行接口）。
fn upsert_with(conn: &Connection, cred: &KiroCredentials) -> anyhow::Result<()> {
    let id = cred.id.context("upsert 凭据缺少 id")? as i64;
    conn.execute(
        "INSERT INTO credentials
            (id, access_token, refresh_token, profile_arn, expires_at, auth_method,
             kiro_api_key, client_id, client_secret, region, auth_region, api_region,
             machine_id, client_mode, proxy_url, proxy_username, proxy_password, group_name,
             email, subscription_title, priority, disabled, rpm_limit, concurrency_limit, overage)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)
         ON CONFLICT(id) DO UPDATE SET
            access_token=excluded.access_token, refresh_token=excluded.refresh_token,
            profile_arn=excluded.profile_arn, expires_at=excluded.expires_at,
            auth_method=excluded.auth_method, kiro_api_key=excluded.kiro_api_key,
            client_id=excluded.client_id, client_secret=excluded.client_secret,
            region=excluded.region, auth_region=excluded.auth_region,
            api_region=excluded.api_region, machine_id=excluded.machine_id,
            client_mode=excluded.client_mode, proxy_url=excluded.proxy_url,
            proxy_username=excluded.proxy_username, proxy_password=excluded.proxy_password,
            group_name=excluded.group_name, email=excluded.email,
            subscription_title=excluded.subscription_title, priority=excluded.priority,
            disabled=excluded.disabled, rpm_limit=excluded.rpm_limit,
            concurrency_limit=excluded.concurrency_limit, overage=excluded.overage",
        params![
            id,
            cred.access_token,
            cred.refresh_token,
            cred.profile_arn,
            expires_unix(cred),
            cred.auth_method,
            cred.kiro_api_key,
            cred.client_id,
            cred.client_secret,
            cred.region,
            cred.auth_region,
            cred.api_region,
            cred.machine_id,
            client_mode_to_str(cred.client_mode),
            cred.proxy_url,
            cred.proxy_username,
            cred.proxy_password,
            cred.group,
            cred.email,
            cred.subscription_title,
            cred.priority as i64,
            cred.disabled as i64,
            cred.rpm_limit.map(|v| v as i64),
            cred.concurrency_limit.map(|v| v as i64),
            cred.overage.map(|b| b as i64),
        ],
    )
    .with_context(|| format!("写入凭据 #{} 失败", id))?;
    Ok(())
}

/// 凭据的过期时间（unix 秒）；解析失败/缺失返回 None。
fn expires_unix(cred: &KiroCredentials) -> Option<i64> {
    cred.expires_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
}

/// unix 秒 → 规范 UTC RFC3339（瞬时值保留，亚秒丢弃，不影响 token 判活）。
fn unix_to_rfc3339(unix: Option<i64>) -> Option<String> {
    unix.and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
        .map(|dt| dt.to_rfc3339())
}

/// `ClientMode` → 列文本（用 serde 的 kebab-case 表示："kiro-ide"/"kiro-cli"）。
fn client_mode_to_str(mode: Option<ClientMode>) -> Option<String> {
    mode.and_then(|m| serde_json::to_value(m).ok())
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

/// 列文本 → `ClientMode`（无法识别按 None 处理，运行时回退 config 默认）。
fn client_mode_from_str(s: Option<String>) -> Option<ClientMode> {
    s.and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CredentialStore {
        // 内存库（共享 cache 关闭即每个连接独立；这里单连接，用 :memory: 即可）。
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        CredentialStore::init_schema(&conn).unwrap();
        CredentialStore { conn: Mutex::new(conn) }
    }

    fn oauth(id: u64, token: &str) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            refresh_token: Some(token.to_string()),
            auth_method: Some("social".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn upsert_and_load_roundtrip() {
        let s = store();
        let mut c = oauth(1, "rt-1");
        c.access_token = Some("at-1".into());
        c.expires_at = Some("2026-01-02T03:04:05+00:00".into());
        c.priority = 7;
        c.disabled = true;
        c.rpm_limit = Some(0);
        c.concurrency_limit = Some(5);
        c.overage = Some(true);
        c.client_mode = Some(ClientMode::KiroCli);
        s.upsert(&c).unwrap();

        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        let g = &all[0];
        assert_eq!(g.id, Some(1));
        assert_eq!(g.access_token.as_deref(), Some("at-1"));
        assert_eq!(g.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(g.priority, 7);
        assert!(g.disabled);
        assert_eq!(g.rpm_limit, Some(0));
        assert_eq!(g.concurrency_limit, Some(5));
        assert_eq!(g.overage, Some(true));
        assert_eq!(g.client_mode, Some(ClientMode::KiroCli));
        // 过期时间瞬时值往返一致
        assert_eq!(expires_unix(g), expires_unix(&c));
    }

    #[test]
    fn upsert_updates_single_row() {
        let s = store();
        s.upsert(&oauth(1, "rt-1")).unwrap();
        let mut c = oauth(1, "rt-1");
        c.access_token = Some("rotated".into());
        s.upsert(&c).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].access_token.as_deref(), Some("rotated"));
    }

    #[test]
    fn load_all_sorted_by_priority() {
        let s = store();
        let mut a = oauth(1, "rt-a");
        a.priority = 5;
        let mut b = oauth(2, "rt-b");
        b.priority = 1;
        s.upsert(&a).unwrap();
        s.upsert(&b).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all[0].id, Some(2)); // priority 1 在前
        assert_eq!(all[1].id, Some(1));
    }

    #[test]
    fn sync_all_deletes_missing() {
        let s = store();
        s.upsert(&oauth(1, "rt-1")).unwrap();
        s.upsert(&oauth(2, "rt-2")).unwrap();
        // 仅保留 #2
        s.sync_all(&[oauth(2, "rt-2")]).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, Some(2));
    }

    #[test]
    fn unique_refresh_token_enforced() {
        let s = store();
        s.upsert(&oauth(1, "dup")).unwrap();
        // 不同 id、相同 refresh_token → UNIQUE 拒绝
        assert!(s.upsert(&oauth(2, "dup")).is_err());
    }

    #[test]
    fn check_rejects_dead_credential() {
        let s = store();
        let dead = KiroCredentials {
            id: Some(1),
            ..Default::default()
        };
        // 既无 refresh_token 又无 kiro_api_key → 表级 CHECK 拒绝
        assert!(s.upsert(&dead).is_err());
    }

    #[test]
    fn migrate_dedups_and_skips_dead() {
        let s = store();
        let mut newer = oauth(0, "same");
        newer.id = None;
        newer.expires_at = Some("2026-06-01T00:00:00+00:00".into());
        let mut older = oauth(0, "same");
        older.id = None;
        older.expires_at = Some("2026-01-01T00:00:00+00:00".into());
        let dead = KiroCredentials::default(); // 无任何鉴权
        let mut apikey = KiroCredentials::default();
        apikey.kiro_api_key = Some("ksk_x".into());
        apikey.auth_method = Some("api_key".into());

        let report = s
            .migrate_from_json(vec![newer, older, dead, apikey])
            .unwrap();
        assert_eq!(report.skipped_dead, 1);
        assert_eq!(report.deduped, 1);
        assert_eq!(report.imported, 2); // 去重后的 oauth + api_key

        let all = s.load_all().unwrap();
        // 保留的是 expires 较新的那条
        let kept = all.iter().find(|c| c.refresh_token.as_deref() == Some("same")).unwrap();
        assert_eq!(kept.expires_at.as_deref(), Some("2026-06-01T00:00:00+00:00"));
    }
}

mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
#[cfg(feature = "import-kiro-cli")]
mod import;
mod kiro;
mod model;
mod stats;
pub mod token;

use std::sync::Arc;

use clap::Parser;
use kiro::credential_store::CredentialStore;
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    //
    // 使用 tracing-appender 的非阻塞写入器：实际的 stdout 写入在后台专用线程完成，
    // async 任务只把日志推入无锁队列即返回，避免在 tokio worker 线程上执行阻塞 write
    // 系统调用阻塞执行器（高并发/流式请求时表现为卡顿）。
    //
    // `_log_guard` 必须存活到 main 结束：Drop 时会 flush 队列中尚未写出的日志，
    // 否则进程退出时可能丢失尾部日志。
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // 仅在输出到真正的终端时启用 ANSI 颜色；重定向到文件/管道时关闭，避免转义码污染日志。
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_writer(non_blocking)
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());

    // 如果指定了 --import-kiro-cli，从 kiro-cli 数据库导入凭据
    #[cfg(feature = "import-kiro-cli")]
    if args.import_kiro_cli {
        let db_path = args
            .kiro_cli_db
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(import::default_db_path);
        tracing::info!("从 kiro-cli 数据库导入凭据: {}", db_path.display());
        match import::import_credentials(&db_path) {
            Ok(cred) => {
                let json = serde_json::to_string_pretty(&cred).unwrap();
                std::fs::write(&credentials_path, &json).unwrap_or_else(|e| {
                    tracing::error!("写入凭据文件失败: {}", e);
                    std::process::exit(1);
                });
                tracing::info!("凭据已导入到: {}", credentials_path);
            }
            Err(e) => {
                tracing::error!("导入凭据失败: {}", e);
                std::process::exit(1);
            }
        }
    }

    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 凭据持久化已迁移到 SQLite（credentials.db，与 credentials.json 同目录）。
    // credentials.json 仅作为「首次迁移」的数据源与 kiro-cli 导入的落点。
    let db_path = {
        let p = std::path::Path::new(&credentials_path);
        match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(dir) => dir.join("kiro.db"),
            None => std::path::PathBuf::from("kiro.db"),
        }
    };
    let store = Arc::new(CredentialStore::open(&db_path).unwrap_or_else(|e| {
        tracing::error!("打开凭据库失败: {}", e);
        std::process::exit(1);
    }));

    // 首次启动（库为空）：把 credentials.json 迁移入库（去重 + 跳过死号）。
    match store.is_empty() {
        Ok(true) => {
            let json_creds = credentials_config.into_sorted_credentials();
            if !json_creds.is_empty() {
                match store.migrate_from_json(json_creds) {
                    Ok(rep) => tracing::info!(
                        "凭据已从 credentials.json 迁移入库: 导入 {}、去重跳过 {}、死号跳过 {}",
                        rep.imported,
                        rep.deduped,
                        rep.skipped_dead
                    ),
                    Err(e) => {
                        tracing::error!("凭据迁移失败: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!("检查凭据库失败: {}", e);
            std::process::exit(1);
        }
    }

    // 凭据真相源 = SQLite，按优先级载入。
    let mut credentials_list = store.load_all().unwrap_or_else(|e| {
        tracing::error!("从凭据库载入失败: {}", e);
        std::process::exit(1);
    });

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        // 去重：库里已有同 kiroApiKey 的凭据则不重复注入（避免每次启动累积）。
        let already = credentials_list
            .iter()
            .any(|c| c.kiro_api_key.as_deref() == Some(kiro_api_key.as_str()));
        if !kiro_api_key.is_empty() && !already {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        Some(store),
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);
    let kiro_provider = KiroProvider::with_proxy(token_manager.clone(), proxy_config.clone());

    // 统一持久化到同目录的 kiro.db：计费累计 + 请求时序统计（与凭据/余额同库）。
    if let Some(dir) = token_manager.cache_dir() {
        anthropic::init_billing_stats(dir.join("kiro.db"));
        // 请求级时序统计：request_stats 表，保留 30 天，供 admin 出曲线。
        stats::init(dir.join("kiro.db"), 30);
    }

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // 解析缓存分桶策略：cache_scope 优先，否则回落到 global_cache 布尔
    let cache_scope = match config.cache_scope.as_deref() {
        Some(s) => anthropic::CacheScope::parse(s),
        None if config.global_cache => anthropic::CacheScope::Global,
        None => anthropic::CacheScope::PerCredential,
    };

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let (anthropic_app, app_state) = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        cache_scope,
        config.cache_skip_rate,
    );

    // 后台拉取上游 ListAvailableModels，动态校准 contextUsage 反推用的上下文窗口；
    // 用上游真实 maxInputTokens 取代硬编码常量，失败时静默回退硬编码。每 6 小时刷新。
    if let Some(provider) = app_state.kiro_provider.clone() {
        tokio::spawn(async move {
            const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
            loop {
                match provider.list_available_models().await {
                    Ok(models) => {
                        // 上游 modelId（点号形式）经 map_model 归一化为窗口表 key。
                        let mut windows = std::collections::HashMap::new();
                        for (id, max) in &models {
                            if let Some(mapped) = anthropic::map_model(id) {
                                windows.insert(mapped, *max);
                            }
                        }
                        let count = windows.len();
                        // 逐个列出上游真实 maxInputTokens，便于核对硬编码是否准确。
                        let mut detail: Vec<String> =
                            windows.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        detail.sort();
                        anthropic::set_dynamic_model_windows(windows);
                        tracing::info!(
                            "动态窗口已更新：{} 个模型（来自上游 ListAvailableModels）[{}]",
                            count,
                            detail.join(", ")
                        );
                    }
                    Err(e) => {
                        tracing::warn!("拉取 ListAvailableModels 失败，沿用硬编码窗口: {}", e);
                    }
                }
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
        });
    }

    // 后台 Token 预刷新：启动立即扫一次，之后每 60s 扫描所有 OAuth 凭据，
    // 把 10 分钟内到期的提前刷掉，让首字请求不再为「正好命中刚过期的凭据」
    // 付出刷新延迟。单凭据有 refresh_lock 串行保护；跨凭据并发控制在 4 路防止
    // 上游集中限流。
    {
        let tm = token_manager.clone();
        tokio::spawn(async move {
            use futures::stream::{self, StreamExt};
            const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
            const REFRESH_CONCURRENCY: usize = 4;
            loop {
                let due = tm.refresh_due_ids();
                if !due.is_empty() {
                    tracing::info!("后台预刷新：{} 个凭据临期，开始刷新", due.len());
                    let tm_inner = tm.clone();
                    let results: Vec<(u64, anyhow::Result<()>)> = stream::iter(due)
                        .map(|id| {
                            let tm = tm_inner.clone();
                            async move { (id, tm.refresh_if_due(id).await) }
                        })
                        .buffer_unordered(REFRESH_CONCURRENCY)
                        .collect()
                        .await;
                    let (ok, err): (Vec<_>, Vec<_>) =
                        results.into_iter().partition(|(_, r)| r.is_ok());
                    if !err.is_empty() {
                        for (id, e) in &err {
                            tracing::warn!(
                                "后台预刷新失败：凭据 #{} {}",
                                id,
                                e.as_ref().err().map(|e| e.to_string()).unwrap_or_default()
                            );
                        }
                    }
                    tracing::info!(
                        "后台预刷新完成：成功 {} 个，失败 {} 个",
                        ok.len(),
                        err.len()
                    );
                }
                tokio::time::sleep(SCAN_INTERVAL).await;
            }
        });
    }

    // 后台清理粘性绑定表：定期回收长时间未活跃的 device→凭证绑定，
    // 防止 binding_table 随独立 device 数只增不减（绑定仅内存维护，无此清理会
    // 随运行时长无界增长）。空闲超过 STALE_IDLE 的绑定下次请求会重新选凭证。
    {
        let binding_table = app_state.binding_table.clone();
        tokio::spawn(async move {
            // 扫描间隔 10 分钟；空闲超过 6 小时的绑定视为过期回收。
            // 6 小时远大于上游 prompt cache TTL（1 小时），回收不会损伤仍在复用缓存的活跃 device。
            const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
            const STALE_IDLE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let removed = binding_table.sweep_stale(STALE_IDLE);
                if removed > 0 {
                    tracing::debug!(
                        "粘性绑定清理：回收 {} 条过期绑定，当前剩余 {} 条",
                        removed,
                        binding_table.len()
                    );
                }
            }
        });
    }

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                app_state.cache_tracker.clone(),
            );
            let guest_keys: Vec<String> = config
                .guest_api_keys
                .iter()
                .filter(|k| !k.trim().is_empty())
                .cloned()
                .collect();
            let guest_count = guest_keys.len();
            let admin_state = admin::AdminState::new(admin_key, guest_keys, admin_service);
            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            if guest_count > 0 {
                tracing::info!("已配置 {} 个 Guest API Key（只读权限）", guest_count);
            }
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // 不用 with_graceful_shutdown：它会等所有在途请求结束才返回，而 LLM 流式响应
    // 可能持续数分钟，会把关停（及重启）阻塞很久，甚至被 supervisor SIGKILL 而漏刷。
    // 改为 select：收到信号即丢弃在途连接、立刻刷盘（亚毫秒）退出，重启不变慢。
    let server = axum::serve(listener, app).into_future();
    tokio::select! {
        res = server => {
            if let Err(e) = res {
                tracing::error!("服务器异常退出: {}", e);
            }
        }
        _ = shutdown_signal() => {
            tracing::info!("收到关停信号，刷盘统计后退出…");
        }
    }

    // 信号触发后强制刷盘统计/计费累计，避免丢失距上次 debounce 落盘的数据
    // （Drop 在信号杀进程时不保证执行：背景任务长期持有 token_manager 的 Arc，
    //  且 billing 统计是 static 单例，析构都不会跑——故必须显式 flush）。
    token_manager.flush_stats();
    anthropic::billing_stats().flush();
    tracing::info!("统计已刷盘，退出");
}

/// 等待 Ctrl-C（SIGINT）或 SIGTERM（容器/systemd 停止）。任一到达即触发优雅关停。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("注册 SIGTERM 处理失败，仅监听 Ctrl-C: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

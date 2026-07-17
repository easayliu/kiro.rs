//! WebSearch 服务端工具（server tool）处理模块
//!
//! web_search 按 Anthropic 官方 server-tool 语义处理：工具透传给模型，由模型自主决定
//! 搜什么、搜几次；模型发起调用后由代理侧打 Kiro MCP 执行搜索，把结果重建为
//! `server_tool_use` + `web_search_tool_result` 块注入响应，并以 `pause_turn` 让客户端
//! 原样回传续跑（续跑侧的还原见 [`split_web_search_tool_results`]）。

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::types::MessagesRequest;

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
    #[serde(rename = "_meta")]
    pub meta: McpArgumentsMeta,
}

/// MCP 参数元信息（对齐 KiroIDE 真实抓包，上游据此校验输入完整性）
#[derive(Debug, Serialize)]
pub struct McpArgumentsMeta {
    #[serde(rename = "_isValid")]
    pub is_valid: bool,
    #[serde(rename = "_activePath")]
    pub active_path: Vec<String>,
    #[serde(rename = "_completedPaths")]
    pub completed_paths: Vec<Vec<String>>,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

/// 判断一个工具定义是否是 Anthropic 的 web_search 服务端工具。
fn is_web_search_tool(t: &super::types::Tool) -> bool {
    t.tool_type
        .as_deref()
        .is_some_and(|s| s.starts_with("web_search"))
        || t.name == "web_search"
}

/// 多工具 tool loop 中的 web_search 服务端工具状态（server-tool / pause_turn 路径）。
///
/// 仅当请求在 tools 里声明了 web_search 时存在——作为响应侧「是否需要拦截并代理执行
/// 搜索」的开关，非 web_search 请求恒为 None、完全不触发新逻辑。
pub struct WebSearchState {
    /// 请求声明的 max_uses（每轮搜索上限）。
    pub max_uses: Option<i32>,
    /// 历史里已经发生过的 web_search 次数（数 assistant 消息里的 server_tool_use(web_search) 块）。
    pub prior_uses: i32,
}

impl WebSearchState {
    /// 是否已达到 max_uses 上限（下一次搜索应返回 max_uses_exceeded 错误、不真正执行）。
    pub fn exceeded(&self) -> bool {
        self.max_uses.is_some_and(|m| self.prior_uses >= m)
    }
}

/// 若请求声明了 web_search 工具，返回其服务端工具状态；否则 None。
///
/// 不限制工具数量：单工具与多工具一致，都走「web_search 作为真 server tool」的
/// pause_turn 路径（早期按 tools.len()==1 分流的单发伪造路径已退休）。
pub fn web_search_state(req: &MessagesRequest) -> Option<WebSearchState> {
    let tools = req.tools.as_ref()?;
    let ws = tools.iter().find(|t| is_web_search_tool(t))?;
    let prior_uses = count_prior_web_searches(req);
    Some(WebSearchState {
        max_uses: ws.max_uses,
        prior_uses,
    })
}

/// 数历史 assistant 消息里的 server_tool_use(web_search) 块个数 = 本轮已发生的搜索次数。
///
/// 无状态地实现 max_uses：每次 pause_turn 续跑，客户端会把带 server_tool_use 的
/// assistant 内容回传，据此累加即可，无需服务端保存计数。
fn count_prior_web_searches(req: &MessagesRequest) -> i32 {
    let mut count = 0i32;
    for msg in &req.messages {
        if msg.role != "assistant" {
            continue;
        }
        if let serde_json::Value::Array(arr) = &msg.content {
            for block in arr {
                let is_ws_server_tool = block.get("type").and_then(|v| v.as_str())
                    == Some("server_tool_use")
                    && block.get("name").and_then(|v| v.as_str()) == Some("web_search");
                if is_ws_server_tool {
                    count += 1;
                }
            }
        }
    }
    count
}

/// pause_turn 续跑预处理：把 assistant 消息里内联的 web_search_tool_result 块拆出来，
/// 转成紧随其后的独立 user 消息（普通 tool_result），使其落入 Kiro 的 tool loop
/// （assistant 出 tool_use、user 回 tool_result）。
///
/// Anthropic 服务端工具协议里 `server_tool_use` 与 `web_search_tool_result` 同处
/// assistant 轮回传；而 Kiro 的 tool_result 必须在独立 user 轮，故按 `tool_use_id`
/// 拆分重排。返回 `None` 表示无需拆分（无 web_search_tool_result），调用方直接用原
/// messages，避免无谓克隆。
pub fn split_web_search_tool_results(
    messages: &[super::types::Message],
) -> Option<Vec<super::types::Message>> {
    let block_is_ws_result = |b: &serde_json::Value| {
        b.get("type").and_then(|v| v.as_str()) == Some("web_search_tool_result")
    };
    let needs_split = messages.iter().any(|m| {
        m.role == "assistant"
            && m.content
                .as_array()
                .is_some_and(|arr| arr.iter().any(block_is_ws_result))
    });
    if !needs_split {
        return None;
    }

    let mut out: Vec<super::types::Message> = Vec::with_capacity(messages.len() + 1);
    for m in messages {
        let split = m.role == "assistant"
            && m.content
                .as_array()
                .is_some_and(|arr| arr.iter().any(block_is_ws_result));
        if !split {
            out.push(m.clone());
            continue;
        }

        let arr = m.content.as_array().unwrap();
        let mut assistant_blocks: Vec<serde_json::Value> = Vec::new();
        let mut tool_result_blocks: Vec<serde_json::Value> = Vec::new();
        for b in arr {
            if block_is_ws_result(b) {
                let tool_use_id = b
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let (text, is_error) = decode_web_search_tool_result(&b.get("content").cloned());
                tool_result_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": text,
                    "is_error": is_error
                }));
            } else {
                assistant_blocks.push(b.clone());
            }
        }

        out.push(super::types::Message {
            role: "assistant".to_string(),
            content: serde_json::Value::Array(assistant_blocks),
        });
        if !tool_result_blocks.is_empty() {
            out.push(super::types::Message {
                role: "user".to_string(),
                content: serde_json::Value::Array(tool_result_blocks),
            });
        }
    }
    Some(out)
}

/// 代理侧执行模型发起的一次 web_search 调用，产出要注入响应的两个 content block：
/// `server_tool_use` + `web_search_tool_result`（server-tool / pause_turn 路径）。
///
/// - `tool_use_id`：复用模型那次 tool_use 的 id（已 `toolu_` 规范化），使续跑回传时
///   与配对的 tool_result 经 `map_tool_use_id` 后仍配得上。
/// - `exceeded`：max_uses 已超限时为 true，不真正搜索，返回 max_uses_exceeded 错误块。
///
/// 返回 `(blocks, billed_requests)`。billed_requests：真正执行了搜索为 1，
/// 出错/超限为 0（对齐官方「搜索出错不计费」）。
pub async fn execute_agentic_web_search(
    provider: &crate::kiro::provider::KiroProvider,
    tool_use_id: &str,
    query: &str,
    preferred: Option<u64>,
    exceeded: bool,
) -> (Vec<serde_json::Value>, i32) {
    let server_tool_use = json!({
        "type": "server_tool_use",
        "id": tool_use_id,
        "name": "web_search",
        "input": { "query": query }
    });

    let (content, billed) = if exceeded {
        (max_uses_exceeded_content(), 0)
    } else {
        run_agentic_search(provider, query, preferred).await
    };

    let result = json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "content": content
    });
    (vec![server_tool_use, result], billed)
}

/// 执行一次代理侧 web_search，返回 `(web_search_tool_result.content, billed_requests)`。
/// content 为搜索结果块数组（出错时为空数组）；billed：成功 1、出错 0（对齐官方「出错不计费」）。
/// 供流式与非流式路径共用（见 [`execute_agentic_web_search`] 与 StreamContext 的注入）。
pub async fn run_agentic_search(
    provider: &crate::kiro::provider::KiroProvider,
    query: &str,
    preferred: Option<u64>,
) -> (serde_json::Value, i32) {
    tracing::info!(query = %query, "代理侧执行 web_search（server-tool/pause_turn 路径）");
    let (_mcp_tool_use_id, mcp_request) = create_mcp_request(query);
    match call_mcp_api(provider, &mcp_request, preferred).await {
        Ok((resp, _cred)) => (
            serde_json::Value::Array(build_search_result_blocks(&parse_search_results(&resp))),
            1,
        ),
        Err(e) => {
            tracing::warn!("agentic web_search MCP 调用失败: {}", e);
            (serde_json::Value::Array(vec![]), 0)
        }
    }
}

/// max_uses 超限时 web_search_tool_result 的错误 content。
pub fn max_uses_exceeded_content() -> serde_json::Value {
    tracing::info!("web_search 达到 max_uses 上限，返回 max_uses_exceeded");
    json!({
        "type": "web_search_tool_result_error",
        "error_code": "max_uses_exceeded"
    })
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}（对齐 KiroIDE 真实抓包）
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let request_id = format!("web_search_tooluse_{}", generate_random_id_22());

    // tool_use_id 使用相同格式
    let tool_use_id = format!(
        "srvtoolu_{}",
        Uuid::new_v4().to_string().replace('-', "")[..32].to_string()
    );

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
                meta: McpArgumentsMeta {
                    is_valid: true,
                    active_path: vec!["query".to_string()],
                    completed_paths: vec![vec!["query".to_string()]],
                },
            },
        },
    };

    (tool_use_id, request)
}

/// 解析 MCP 响应中的搜索结果
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    let result = mcp_response.result.as_ref()?;
    let content = result.content.first()?;

    if content.content_type != "text" {
        return None;
    }

    serde_json::from_str(&content.text).ok()
}

/// 构建 web_search_tool_result 的 content（搜索结果块数组）
///
/// 流式与非流式响应共用，确保两条路径结果结构一致。
fn build_search_result_blocks(search_results: &Option<WebSearchResults>) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    match search_results {
        Some(results) => results
            .results
            .iter()
            .map(|r| {
                let page_age = r.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                // encrypted_content 官方语义是不透明加密块、客户端原样回传。这里把 snippet
                // base64 编码放入，使 server-tool 续跑(pause_turn)时能自解回文本喂回 Kiro，
                // 无需服务端保存状态（见 decode_web_search_tool_result）。
                let encrypted_content = base64::engine::general_purpose::STANDARD
                    .encode(r.snippet.clone().unwrap_or_default());
                json!({
                    "type": "web_search_result",
                    "title": r.title,
                    "url": r.url,
                    "encrypted_content": encrypted_content,
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// 把续跑回传的 web_search_tool_result 内容还原成喂回 Kiro 的 tool_result 文本。
///
/// 返回 `(文本, is_error)`。正常结果按每条 `title/url` + base64 解码后的
/// `encrypted_content`(snippet) 重建；`web_search_tool_result_error` 对象则返回错误文本。
/// 与 [`build_search_result_blocks`] 的编码互为逆过程，保持无状态。
pub fn decode_web_search_tool_result(content: &Option<serde_json::Value>) -> (String, bool) {
    use base64::Engine as _;
    let Some(content) = content else {
        return (String::new(), false);
    };

    // 错误对象：{ "type": "web_search_tool_result_error", "error_code": "..." }
    if let Some(obj) = content.as_object() {
        if obj.get("type").and_then(|v| v.as_str()) == Some("web_search_tool_result_error") {
            let code = obj
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return (format!("Web search failed (error_code: {}).", code), true);
        }
    }

    let Some(arr) = content.as_array() else {
        // 兜底：非数组非错误对象，原样字符串化
        return (content.to_string(), false);
    };

    let mut parts: Vec<String> = Vec::new();
    for (i, block) in arr.iter().enumerate() {
        let title = block.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let url = block.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let snippet = block
            .get("encrypted_content")
            .and_then(|v| v.as_str())
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default();
        let mut entry = format!("[{}] {}\n{}", i + 1, title, url);
        if !snippet.is_empty() {
            entry.push('\n');
            entry.push_str(&snippet);
        }
        parts.push(entry);
    }

    if parts.is_empty() {
        ("No search results found.".to_string(), false)
    } else {
        (parts.join("\n\n"), false)
    }
}

/// 调用 Kiro MCP API，返回 MCP 响应体与实际服务该请求的 credential_id
async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
    preferred: Option<u64>,
) -> anyhow::Result<(McpResponse, u64)> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let api_result = provider.call_mcp(&request_body, preferred).await?;
    let credential_id = api_result.credential_id;

    let body = api_result.response.text().await?;
    tracing::debug!("MCP response: {}", body);

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok((mcp_response, credential_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}（对齐 KiroIDE 真实抓包，无时间戳/8位后缀）
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        // 后缀应是单段 22 位大小写字母和数字，不含下划线
        assert_eq!(suffix.len(), 22);
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }
    #[test]
    fn test_decode_web_search_tool_result_roundtrip() {
        // build_search_result_blocks 编码 → decode 解码，snippet 应经 base64 往返还原
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust 2026".to_string(),
                url: "https://rust-lang.org".to_string(),
                snippet: Some("Rust is a systems language.".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let blocks = build_search_result_blocks(&Some(results));
        // encrypted_content 应为 base64（非明文 snippet）
        use base64::Engine as _;
        let enc = blocks[0]["encrypted_content"].as_str().unwrap();
        assert!(base64::engine::general_purpose::STANDARD.decode(enc).is_ok());

        let content = serde_json::Value::Array(blocks);
        let (text, is_error) = decode_web_search_tool_result(&Some(content));
        assert!(!is_error);
        assert!(text.contains("Rust 2026"));
        assert!(text.contains("https://rust-lang.org"));
        assert!(text.contains("Rust is a systems language."));
    }

    #[test]
    fn test_decode_web_search_tool_result_error() {
        let content = serde_json::json!({
            "type": "web_search_tool_result_error",
            "error_code": "max_uses_exceeded"
        });
        let (text, is_error) = decode_web_search_tool_result(&Some(content));
        assert!(is_error);
        assert!(text.contains("max_uses_exceeded"));
    }

    #[test]
    fn test_web_search_state_and_max_uses() {
        use crate::anthropic::types::{Message, Tool};

        // 历史里有一次 server_tool_use(web_search) → prior_uses=1
        let req = MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!("search rust"),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        { "type": "text", "text": "searching" },
                        { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "rust"} },
                        { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] }
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(1),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let state = web_search_state(&req).expect("web_search 已声明，应有 state");
        assert_eq!(state.prior_uses, 1);
        assert_eq!(state.max_uses, Some(1));
        // prior_uses(1) >= max_uses(1) → 已超限
        assert!(state.exceeded());
    }

    #[test]
    fn test_web_search_state_none_when_absent() {
        use crate::anthropic::types::{Message, Tool};
        let req = MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: None,
            tools: Some(vec![Tool {
                tool_type: None,
                name: "other_tool".to_string(),
                description: "x".to_string(),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert!(web_search_state(&req).is_none());
    }
}

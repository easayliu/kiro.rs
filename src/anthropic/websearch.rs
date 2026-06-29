//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成

use std::convert::Infallible;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use std::sync::Arc;

use super::stream::SseEvent;
use super::types::{ErrorResponse, MessagesRequest};
use crate::kiro::binding::BindingTable;

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

/// 检查请求是否为纯 WebSearch 请求
///
/// 条件：tools 有且只有一个，且 name 为 web_search
pub fn has_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools.len() == 1 && tools.first().is_some_and(|t| t.name == "web_search")
    })
}

/// 从消息中提取搜索查询
///
/// 读取 messages 的第一条消息的第一个内容块
/// 并去除 "Perform a web search for the query: " 前缀
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    // 获取第一条消息
    let first_msg = req.messages.first()?;

    // 提取文本内容
    let text = match &first_msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 获取第一个内容块
            let first_block = arr.first()?;
            if first_block.get("type")?.as_str()? == "text" {
                first_block.get("text")?.as_str()?.to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // 去除前缀 "Perform a web search for the query: "
    const PREFIX: &str = "Perform a web search for the query: ";
    let query = if text.starts_with(PREFIX) {
        text[PREFIX.len()..].to_string()
    } else {
        text
    };

    if query.is_empty() { None } else { Some(query) }
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

/// 生成 WebSearch SSE 响应流
pub fn create_websearch_sse_stream(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let events =
        generate_websearch_events(&model, &query, &tool_use_id, search_results, input_tokens);

    stream::iter(
        events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    )
}

/// 构建 web_search_tool_result 的 content（搜索结果块数组）
///
/// 流式与非流式响应共用，确保两条路径结果结构一致。
fn build_search_result_blocks(search_results: &Option<WebSearchResults>) -> Vec<serde_json::Value> {
    match search_results {
        Some(results) => results
            .results
            .iter()
            .map(|r| {
                let page_age = r.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": r.title,
                    "url": r.url,
                    "encrypted_content": r.snippet.clone().unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// 构建最终文本块的 citations（web_search_result_location 数组）
///
/// 官方 web search 回答的 text 块必带引用。Kiro 不提供 encrypted_index，
/// 用确定性 base64 占位（满足存在性/格式校验，无法通过密码学校验）。
fn build_citations(search_results: &Option<WebSearchResults>) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    match search_results {
        Some(results) => results
            .results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let cited = r.snippet.clone().unwrap_or_default();
                // cited_text 官方上限约 150 字符（安全处理 UTF-8）
                let cited_text = match cited.char_indices().nth(150) {
                    Some((idx, _)) => cited[..idx].to_string(),
                    None => cited,
                };
                let encrypted_index = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", i + 1, r.url));
                json!({
                    "type": "web_search_result_location",
                    "url": r.url,
                    "title": r.title,
                    "encrypted_index": encrypted_index,
                    "cited_text": cited_text
                })
            })
            .collect(),
        None => vec![],
    }
}

/// 生成 WebSearch 非流式 JSON 响应（stream:false 时使用）
///
/// 与 SSE 路径同构：text 决策块 + server_tool_use + web_search_tool_result + text 摘要，
/// 聚合为官方 Messages 非流式响应体。
fn create_websearch_json_response(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> serde_json::Value {
    let message_id = format!(
        "msg_{}",
        Uuid::new_v4().to_string().replace('-', "")[..24].to_string()
    );
    let decision_text = format!("I'll search for \"{}\".", query);
    let search_content = build_search_result_blocks(&search_results);
    let citations = build_citations(&search_results);
    let summary = generate_search_summary(query, &search_results);
    let output_tokens = (summary.len() as i32 + 3) / 4;

    // 最终文本块带 citations（官方 web search 回答必有引用）；无结果时省略该字段
    let mut answer_block = json!({ "type": "text", "text": summary });
    if !citations.is_empty() {
        answer_block["citations"] = json!(citations);
    }

    json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [
            { "type": "text", "text": decision_text },
            {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": "web_search",
                "input": { "query": query }
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content
            },
            answer_block
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "server_tool_use": { "web_search_requests": 1 }
        }
    })
}

/// 生成 WebSearch SSE 事件序列
fn generate_websearch_events(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        Uuid::new_v4().to_string().replace('-', "")[..24].to_string()
    );

    // 1. message_start
    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    ));

    // 2. content_block_start (text - 搜索决策说明, index 0)
    let decision_text = format!("I'll search for \"{}\".", query);
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": decision_text
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 0
        }),
    ));

    // 3. content_block_start (server_tool_use, index 1)
    // 官方流式格式：server_tool_use 的 input 通过 input_json_delta 增量传输，
    // content_block_start 中 input 为空对象，与客户端 tool_use 一致。
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search",
                "input": {}
            }
        }),
    ));

    // 4. content_block_delta (input_json_delta) - 查询参数增量
    // 标准 SDK 从空 input 起、按 partial_json 累积，缺此 delta 会重建出空 query。
    let input_json = json!({ "query": query }).to_string();
    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {
                "type": "input_json_delta",
                "partial_json": input_json
            }
        }),
    ));

    // 5. content_block_stop (server_tool_use)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 1
        }),
    ));

    // 6. content_block_start (web_search_tool_result, index 2)
    // 官方格式：web_search_tool_result 带 tool_use_id，与 server_tool_use.id 对应。
    let search_content = build_search_result_blocks(&search_results);

    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content
            }
        }),
    ));

    // 7. content_block_stop (web_search_tool_result)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 2
        }),
    ));

    // 7. content_block_start (text, index 3)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 3,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    // 8. content_block_delta (text_delta) - 生成搜索结果摘要
    let summary = generate_search_summary(query, &search_results);

    // 分块发送文本
    let chunk_size = 100;
    for chunk in summary.chars().collect::<Vec<_>>().chunks(chunk_size) {
        let text: String = chunk.iter().collect();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 3,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ));
    }

    // 8b. citations_delta - 官方 web search 回答的引用，逐条随文本块发送
    for citation in build_citations(&search_results) {
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 3,
                "delta": {
                    "type": "citations_delta",
                    "citation": citation
                }
            }),
        ));
    }

    // 9. content_block_stop (text)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 3
        }),
    ));

    // 10. message_delta
    // 官方 API 的 message_delta.delta 中没有 stop_sequence 字段
    let output_tokens = (summary.len() as i32 + 3) / 4; // 简单估算
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn"
            },
            "usage": {
                "output_tokens": output_tokens,
                "server_tool_use": {
                    "web_search_requests": 1
                }
            }
        }),
    ));

    // 11. message_stop
    events.push(SseEvent::new(
        "message_stop",
        json!({
            "type": "message_stop"
        }),
    ));

    events
}

/// 生成搜索结果摘要
fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
    binding_table: Arc<BindingTable>,
    binding_key: Option<u64>,
) -> Response {
    // [临时诊断] 打印完整入站请求，定位第三方检测的期望（system/messages/tools/stream）
    tracing::info!(target: "websearch_capture", "WebSearch 入站请求: {:#?}", payload);

    // 1. 提取搜索查询
    let query = match extract_search_query(payload) {
        Some(q) => q,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response();
        }
    };

    tracing::info!(query = %query, "处理 WebSearch 请求");

    // 2. 创建 MCP 请求
    let (tool_use_id, mcp_request) = create_mcp_request(&query);

    // 3. 粘性绑定：解析 preferred 凭证（MCP 不按模型过滤，传 None）
    let preferred = binding_key
        .map(|id| (id, provider.available_credential_ids(None)))
        .and_then(|(id, available)| binding_table.resolve(id, &available));

    // 4. 调用 Kiro MCP API
    let (search_results, actual_credential) =
        match call_mcp_api(&provider, &mcp_request, preferred).await {
            Ok((response, cred)) => (parse_search_results(&response), Some(cred)),
            Err(e) => {
                tracing::warn!("MCP API 调用失败: {}", e);
                (None, None)
            }
        };

    // 5. 绑定维护：actual != preferred 说明 preferred 失败/不可用
    maintain_binding(
        &binding_table,
        &provider,
        binding_key,
        preferred,
        actual_credential,
    );

    // 6. 按 stream 标志生成响应：非流式聚合为单个 JSON，流式走 SSE
    let model = payload.model.clone();
    if !payload.stream {
        let body =
            create_websearch_json_response(&model, &query, &tool_use_id, search_results, input_tokens);
        return (StatusCode::OK, Json(body)).into_response();
    }

    let stream =
        create_websearch_sse_stream(model, query, tool_use_id, search_results, input_tokens);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
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

/// 根据本次调用结果维护粘性绑定（与 handlers::update_binding_after_call 同构）
fn maintain_binding(
    binding_table: &BindingTable,
    provider: &crate::kiro::provider::KiroProvider,
    binding_key: Option<u64>,
    preferred: Option<u64>,
    actual: Option<u64>,
) {
    let (identity, pref) = match (binding_key, preferred) {
        (Some(i), Some(p)) => (i, p),
        _ => return,
    };
    let preferred_failed = match actual {
        Some(used) => used != pref,
        None => true,
    };
    if !preferred_failed {
        return;
    }
    if binding_table.report_error(pref) {
        let available = provider.available_credential_ids(None);
        if let Some(new_cred) = binding_table.rebind(identity, pref, &available) {
            tracing::info!(
                identity = identity,
                from = pref,
                to = new_cred,
                "粘性绑定改绑（websearch 路径）：preferred 凭证累计错误达阈值"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_web_search_tool_only_one() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_has_web_search_tool_multiple_tools() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                    cache_control: None,
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // 多个工具时不应该被识别为纯 websearch 请求
        assert!(!has_web_search_tool(&req));
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

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
    fn test_create_websearch_json_response() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("q".to_string()),
            error: None,
        };
        let v = create_websearch_json_response(
            "claude-sonnet-4-5",
            "q",
            "srvtoolu_abc",
            Some(results),
            42,
        );

        // 顶层为完整 message 对象（非流式），非 SSE
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["server_tool_use"]["web_search_requests"], 1);

        let content = v["content"].as_array().expect("content 数组");
        assert_eq!(content.len(), 4);
        assert_eq!(content[1]["type"], "server_tool_use");
        assert_eq!(content[1]["id"], "srvtoolu_abc");
        assert_eq!(content[2]["type"], "web_search_tool_result");
        // tool_use_id 与 server_tool_use.id 关联
        assert_eq!(content[2]["tool_use_id"], "srvtoolu_abc");
        assert_eq!(content[2]["content"][0]["type"], "web_search_result");
        assert_eq!(content[2]["content"][0]["url"], "https://example.com");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }
}

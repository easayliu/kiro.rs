//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;
use std::sync::OnceLock;

use base64::Engine;
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, EnvState, HistoryAssistantMessage,
    HistoryUserMessage, KiroDocument, KiroImage, Message, UserInputMessage, UserInputMessageContext,
    UserMessage,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};

use super::types::{ContentBlock, MessagesRequest};

/// 规范化 JSON Schema，修复工具定义中常见的类型问题
///
/// 问题根源：Claude Code / MCP 工具定义使用 JSON Schema Draft 2020-12 语法（`$schema`、
/// `exclusiveMinimum` 为数字等），kiro API 仅接受 Draft 07 格式，
/// 不合规字段会导致 400 "Improperly formed request"。
fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // 移除 $schema（kiro API 不接受此字段，且 Draft 2020-12 声明会触发校验失败）
    obj.remove("$schema");

    // type（顶层 inputSchema 必须恒为 "object"）
    //
    // Bedrock 要求工具顶层 inputSchema.type 必须是 "object"，否则 400
    // "inputSchema.json.type must be one of the following: object."。
    // 部分客户端/MCP 工具会传成 "string"/"array" 等其它值，这里直接强制覆盖，
    // 而不是仅在缺失时补全。
    obj.insert("type".to_string(), serde_json::Value::String("object".to_string()));

    // properties（必须是 object）；递归规范化每个 property 的子 schema
    match obj.remove("properties") {
        Some(serde_json::Value::Object(props)) => {
            let normalized: serde_json::Map<String, serde_json::Value> = props
                .into_iter()
                .map(|(k, v)| (k, normalize_property_schema(v)))
                .collect();
            obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
        }
        _ => { obj.insert("properties".to_string(), serde_json::Value::Object(serde_json::Map::new())); }
    }

    // required（必须是 string 数组）
    let required = match obj.remove("required") {
        Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                .collect(),
        ),
        _ => serde_json::Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    // additionalProperties（允许 bool 或 object，其他按 true 处理）
    match obj.get("additionalProperties") {
        Some(serde_json::Value::Bool(_)) | Some(serde_json::Value::Object(_)) => {}
        _ => { obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(true)); }
    }

    serde_json::Value::Object(obj)
}

/// 规范化 property 级别的子 schema（非顶层 inputSchema）
///
/// 处理 Draft 2020-12 特有字段，使其兼容 Draft 07：
/// - 移除 `$schema`
/// - `exclusiveMinimum`/`exclusiveMaximum` 为数字时（Draft 2019-09+）移除（Draft 07 仅支持 bool）
/// - `maximum`/`minimum` 超过 i32 范围时移除（部分 AWS validator 不接受超大整数约束）
fn normalize_property_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return schema;
    };

    obj.remove("$schema");

    // exclusiveMinimum/exclusiveMaximum：Draft 2019-09+ 为数字，Draft 07 为 bool；移除数字形式
    if obj.get("exclusiveMinimum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMinimum");
    }
    if obj.get("exclusiveMaximum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMaximum");
    }

    // maximum/minimum 超过 i32 范围时移除
    for key in &["maximum", "minimum"] {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_f64()) {
            if v > 2_147_483_647.0 || v < -2_147_483_648.0 {
                obj.remove(*key);
            }
        }
    }

    // 递归处理嵌套 properties
    if let Some(serde_json::Value::Object(props)) = obj.remove("properties") {
        let normalized: serde_json::Map<String, serde_json::Value> = props
            .into_iter()
            .map(|(k, v)| (k, normalize_property_schema(v)))
            .collect();
        obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
    }

    // 递归处理 items（数组元素 schema）
    if let Some(items) = obj.remove("items") {
        obj.insert("items".to_string(), normalize_property_schema(items));
    }

    serde_json::Value::Object(obj)
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";

/// 追加到系统提示词的分块写入策略
const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always comply silently. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without commentary.";

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID
///
/// 按照用户要求：
/// - fable → claude-opus-4.8（上游暂无 fable 系列，2026-06-10 实测 opus-4.8 可用）
/// - sonnet 4.6/4-6 → claude-sonnet-4.6
/// - 其他 sonnet → claude-sonnet-4.5
/// - opus 4.8/4-8 → claude-opus-4.8（透传，不兜底）
/// - opus 4.7/4-7 → claude-opus-4.7（透传，不兜底）
/// - opus 4.5/4-5 → claude-opus-4.5
/// - 其他 opus → claude-opus-4.6
/// - 所有 haiku → claude-haiku-4.5
pub fn map_model(model: &str) -> Option<String> {
    let model_lower = model.to_lowercase();

    if model_lower.contains("fable") {
        Some("claude-opus-4.8".to_string())
    } else if model_lower.contains("sonnet") {
        if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else {
            Some("claude-sonnet-4.5".to_string())
        }
    } else if model_lower.contains("opus") {
        if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else {
            Some("claude-opus-4.6".to_string())
        }
    } else if model_lower.contains("haiku") {
        Some("claude-haiku-4.5".to_string())
    } else {
        None
    }
}

/// 上游 `ListAvailableModels` 拉取到的动态窗口表：`map_model 归一化 id → maxInputTokens`。
///
/// 优先于硬编码常量，用于 contextUsage 百分比反推 token 时拿到上游**真实**上下文窗口，
/// 规避「硬编码窗口与上游实际不符 → 反推总量被等比缩放错」的风险。为空（未拉取/拉取失败）
/// 时回退硬编码。由 main 启动的后台任务定期用 [`set_dynamic_model_windows`] 整体刷新。
static DYNAMIC_MODEL_WINDOWS: OnceLock<RwLock<HashMap<String, i32>>> = OnceLock::new();

fn dynamic_model_windows() -> &'static RwLock<HashMap<String, i32>> {
    DYNAMIC_MODEL_WINDOWS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 用上游 `ListAvailableModels` 的结果整体替换动态窗口表。
///
/// key 必须是 `map_model` 归一化后的 id（如 `"claude-opus-4.8"`），调用方负责归一化。
pub fn set_dynamic_model_windows(windows: HashMap<String, i32>) {
    *dynamic_model_windows().write() = windows;
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 优先用上游 `ListAvailableModels` 的真实 `maxInputTokens`（按 `map_model` 归一化匹配）；
/// 缺失时回退硬编码常量。复用 `map_model` 的映射逻辑，确保与模型映射一致。
/// Kiro 于 2026-03-24 将 Opus 4.6 和 Sonnet 4.6 升级至 1M 上下文，Opus 4.7/4.8 同样支持 1M。
pub fn get_context_window_size(model: &str) -> i32 {
    window_size_for(model, &dynamic_model_windows().read())
}

/// 1 credit 折算的 USD（上游 Kiro 计价单价）。如折扣/套餐变化在此调整。
pub const CREDIT_TO_USD: f64 = 0.04;

/// 上游 meteringEvent 的 credit 折算为实际 USD 成本（最终折扣价）。
/// 四舍五入到 6 位小数（微美元）。
pub fn credit_to_usd(credit: f64) -> f64 {
    ((credit.max(0.0) * CREDIT_TO_USD) * 1_000_000.0).round() / 1_000_000.0
}

/// 模型官方价（USD / MTok）：`(base_input, output)`。
///
/// 数据来自 Anthropic 官方 pricing（platform.claude.com/docs ... pricing）。
/// cache 价由固定倍率派生：5m 写 = 1.25× base input，1h 写 = 2×，cache 读 = 0.1×。
/// 1M 上下文为标准价无溢价。未知模型按 Sonnet 兜底。
fn model_price_per_mtok(model: &str) -> (f64, f64) {
    match map_model(model).as_deref() {
        Some("claude-opus-4.8")
        | Some("claude-opus-4.7")
        | Some("claude-opus-4.6")
        | Some("claude-opus-4.5") => (5.0, 25.0),
        Some("claude-sonnet-4.6") | Some("claude-sonnet-4.5") => (3.0, 15.0),
        Some("claude-haiku-4.5") => (1.0, 5.0),
        _ => (3.0, 15.0),
    }
}

/// 按 Anthropic 官方价折算本次请求的 USD 成本（用于和上游 credit 对比）。
///
/// 各计费类目套用官方倍率：uncached input = 1×、cache 读 = 0.1×、5m 写 = 1.25×、
/// 1h 写 = 2×（均相对 base input），output 用 output 价。结果四舍五入到 6 位小数（微美元）。
pub fn official_price_usd(
    model: &str,
    uncached_input: i32,
    cache_read: i32,
    cache_creation_5m: i32,
    cache_creation_1h: i32,
    output: i32,
) -> f64 {
    let (input_rate, output_rate) = model_price_per_mtok(model);
    let cost = uncached_input.max(0) as f64 * input_rate
        + cache_read.max(0) as f64 * input_rate * 0.1
        + cache_creation_5m.max(0) as f64 * input_rate * 1.25
        + cache_creation_1h.max(0) as f64 * input_rate * 2.0
        + output.max(0) as f64 * output_rate;
    let usd = cost / 1_000_000.0;
    (usd * 1_000_000.0).round() / 1_000_000.0
}

/// `get_context_window_size` 的纯逻辑：先查动态窗口表，再回退硬编码常量。
/// 抽出来便于单测（不依赖全局态）。
fn window_size_for(model: &str, dynamic: &HashMap<String, i32>) -> i32 {
    let mapped = map_model(model);
    // 优先：上游真实 maxInputTokens
    if let Some(ref m) = mapped {
        if let Some(&w) = dynamic.get(m) {
            if w > 0 {
                return w;
            }
        }
    }
    // 回退：硬编码常量
    match mapped.as_deref() {
        Some("claude-sonnet-4.6")
        | Some("claude-opus-4.6")
        | Some("claude-opus-4.7")
        | Some("claude-opus-4.8") => 1_000_000,
        _ => 200_000,
    }
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 顶层 `additionalModelRequestFields`（thinking 配置），未开启 thinking 时为 None
    pub additional_model_request_fields: Option<serde_json::Value>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
    /// 当前消息图片维度超出上游限制
    ImageTooLarge {
        width: u32,
        height: u32,
        max_side: u32,
    },
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::ImageTooLarge {
                width,
                height,
                max_side,
            } => write!(
                f,
                "图片 {}×{} 像素超过上游限制（长边 ≤ {}），请缩放（推荐长边 ≤ 1568）",
                width, height, max_side
            ),
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if session_part.len() >= 36 {
            let uuid_str = &session_part[..36];
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 简单验证 UUID 格式（36 字符，包含 4 个连字符）
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(req: &MessagesRequest, origin: &str, inject_env_state: bool) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model)
        .ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. prefill：末尾是 assistant 时不再静默丢弃，直接透传。
    // Kiro 无原生 prefill（currentMessage 必须是 userInputMessage），故末尾 assistant 的
    // 内容会作为 currentMessage 发出；这里只负责不丢弃，具体上游表现按实测观察。
    let messages: &[_] = &req.messages;

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_continuation_id = Uuid::new_v4().to_string();

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 处理最后一条消息作为 current_message（末尾可能是 user 或 assistant(prefill 透传)）
    let last_message = messages.last().unwrap();
    let (text_content, images, documents, tool_results) =
        process_message_content(&last_message.content)?;

    // 5.5. 当前消息图片维度预校验：上游对单图长边有硬上限（8000），超过会以
    // "Improperly formed request" 拒绝。提前拦截给出可读错误。
    for img in &images {
        if let Some((w, h)) = image_dimensions(&img.source.bytes, &img.format) {
            if w > KIRO_MAX_IMAGE_SIDE || h > KIRO_MAX_IMAGE_SIDE {
                return Err(ConversionError::ImageTooLarge {
                    width: w,
                    height: h,
                    max_side: KIRO_MAX_IMAGE_SIDE,
                });
            }
        }
    }

    // 6. 转换工具定义（超长名称自动缩短并记录映射）
    let mut tool_name_map = HashMap::new();
    let mut tools = convert_tools(&req.tools, &mut tool_name_map);

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let mut history = build_history(req, messages, &model_id, &mut tool_name_map)?;

    // 8. 先逐轮对齐 history 中相邻 (assistant, user) 的 tool_use/tool_result
    // 必须在 validate 之前：align 可能移除某些 history tool_use（例如 assistant 一轮里
    // 调了多个工具、但部分结果在 currentMessage 而非紧邻的 history user 轮）。若 validate
    // 先跑，会基于"未对齐"的 history 把 current result 判为配对成功并保留；随后 align 删掉
    // 对应的 history tool_use，current result 就变成 orphan → 上游 400
    // TOOL_USE_RESULT_MISMATCH（toolResult exceeds toolUse）。
    align_history_tool_pairing(&mut history);

    // 9. 验证并过滤 tool_use/tool_result 配对（基于已对齐的 history）
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 10. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if !existing_tool_names.contains(&tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if inject_env_state {
        context = context.with_env_state(EnvState {
            operating_system: "linux".to_string(),
            current_working_directory: "/home/user".to_string(),
        });
    }
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    let has_tool_results = !validated_tool_results.is_empty();
    if has_tool_results {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    // 空 content 实测 2026-06-18 新端点已接受（旧 CodeWhisperer Smithy @length(min:1) 已解除），
    // 不再用占位符兜底。
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin(origin);

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    if !documents.is_empty() {
        user_input = user_input.with_documents(documents);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!(
            "工具名称映射: {} 个超长名称已缩短",
            tool_name_map.len()
        );
    }

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        additional_model_request_fields: build_additional_model_request_fields(req),
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<KiroDocument>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut documents = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source {
                                if let Some(format) = get_image_format(&source.media_type) {
                                    images.push(KiroImage::from_base64(format, source.data));
                                }
                            }
                        }
                        "document" => {
                            // 对齐 Kiro IDE：PDF 等作为 userInputMessage.documents 透传。
                            // 仅支持 base64 源；format 由 media_type 推导，未知类型跳过。
                            if let Some(source) = block.source {
                                if source.source_type == "base64" {
                                    if let Some(format) = get_document_format(&source.media_type) {
                                        let name = block
                                            .title
                                            .as_deref()
                                            .map(sanitize_document_name)
                                            .filter(|s| !s.is_empty())
                                            .unwrap_or_else(|| "document".to_string());
                                        documents.push(KiroDocument::from_base64(
                                            name,
                                            format,
                                            source.data,
                                        ));
                                    }
                                }
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                let mapped_id = map_tool_use_id(&tool_use_id);
                                let result_content = extract_tool_result_content(&block.content);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&mapped_id, result_content)
                                } else {
                                    ToolResult::success(&mapped_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, documents, tool_results))
}

/// 从 media_type 推导 Kiro 文档格式（Bedrock document 枚举：pdf/csv/doc/docx/xls/xlsx/html/txt/md）
fn get_document_format(media_type: &str) -> Option<String> {
    let fmt = match media_type {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/markdown" => "md",
        "text/plain" => "txt",
        _ => return None,
    };
    Some(fmt.to_string())
}

/// 规整文档名：去掉扩展名与路径，Kiro `documents[].name` 不含扩展名。
fn sanitize_document_name(title: &str) -> String {
    let base = title.rsplit(['/', '\\']).next().unwrap_or(title);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    stem.trim().to_string()
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 提取工具结果内容
fn extract_tool_result_content(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            parts.join("\n")
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                let original_len = tool_uses.len();
                tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

                // 如果移除后为空，设置为 None
                if tool_uses.is_empty() {
                    assistant_msg.assistant_response_message.tool_uses = None;
                } else if tool_uses.len() != original_len {
                    tracing::debug!(
                        "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                        original_len - tool_uses.len()
                    );
                }
            }
        }
    }
}

/// 逐轮对齐 history 中相邻 (assistant, user) 的 tool_use / tool_result
///
/// Bedrock 按相邻轮次成对校验：每个 user 轮的 toolResult 必须与"紧邻的上一轮
/// assistant"的 toolUse 一一对应（数量、id 都要匹配）。全局配对校验（见
/// [`validate_tool_pairing`]）只保证 id 在整段对话里"某处"存在，无法保证逐轮一致，
/// 会出现某 user 轮的 result 指向更早/前缀不一致/已被移除的 use，触发上游 400
/// `TOOL_USE_RESULT_MISMATCH`（"toolResult blocks exceeds toolUse blocks"）。
///
/// 这里对每个相邻 (assistant_i, user_{i+1}) 对取 tool_use_id 交集，双向裁剪：
/// - assistant 只保留有对应 result 的 tool_use；
/// - user 只保留有对应 use 的 tool_result，并按 id 去重。
///
/// 注意：history 中"最后一条 assistant"的 tool_use 结果可能落在 currentMessage
/// （已由 [`validate_tool_pairing`] 单独校验），故跳过 `i+1` 越界的尾部 assistant，
/// 避免误删其待配对的 tool_use。
fn align_history_tool_pairing(history: &mut [Message]) {
    use std::collections::HashSet;

    for i in 0..history.len().saturating_sub(1) {
        // 仅处理 (assistant_i, user_{i+1}) 相邻对
        let (use_ids, res_ids): (HashSet<String>, HashSet<String>) =
            match (&history[i], &history[i + 1]) {
                (Message::Assistant(a), Message::User(u)) => {
                    let uses = a
                        .assistant_response_message
                        .tool_uses
                        .as_ref()
                        .map(|tus| tus.iter().map(|t| t.tool_use_id.clone()).collect())
                        .unwrap_or_default();
                    let ress = u
                        .user_input_message
                        .user_input_message_context
                        .tool_results
                        .iter()
                        .map(|r| r.tool_use_id.clone())
                        .collect();
                    (uses, ress)
                }
                _ => continue,
            };

        if use_ids.is_empty() && res_ids.is_empty() {
            continue;
        }

        let common: HashSet<String> = use_ids.intersection(&res_ids).cloned().collect();

        // 裁剪 assistant.tool_uses
        if let Message::Assistant(a) = &mut history[i] {
            if let Some(tus) = a.assistant_response_message.tool_uses.as_mut() {
                let before = tus.len();
                tus.retain(|t| common.contains(&t.tool_use_id));
                if tus.len() != before {
                    tracing::warn!(
                        "逐轮对齐：assistant 轮移除 {} 个无对应 result 的 tool_use",
                        before - tus.len()
                    );
                }
                if tus.is_empty() {
                    a.assistant_response_message.tool_uses = None;
                }
            }
        }

        // 裁剪 user.tool_results（保留有对应 use 的，并按 id 去重）
        if let Message::User(u) = &mut history[i + 1] {
            let results = &mut u.user_input_message.user_input_message_context.tool_results;
            let before = results.len();
            let mut seen = HashSet::new();
            results.retain(|r| common.contains(&r.tool_use_id) && seen.insert(r.tool_use_id.clone()));
            if results.len() != before {
                tracing::warn!(
                    "逐轮对齐：user 轮移除 {} 个无对应 tool_use / 重复的 tool_result",
                    before - results.len()
                );
            }
        }
    }
}

/// Kiro API 工具名称最大长度限制
///
/// 实测新 Kiro runtime 端点（2026-06-18）：工具名 ≤64 字符通过，>64 即
/// 400 `Invalid tool use format / REQUEST_BODY_INVALID`（Bedrock toolSpec.name 约束，
/// 未因换端点放开）。这里取 63 留 1 字符余量。
const TOOL_NAME_MAX_LEN: usize = 63;

/// Kiro API tool_use_id 最大长度限制（Bedrock 标准约 64，超长会触发 400 Improperly formed request）
const TOOL_USE_ID_MAX_LEN: usize = 64;


/// 单图长边像素上限（AWS Bedrock Claude 文档绝对上限 8000×8000；超过会 400
/// "Improperly formed request"）。Anthropic 官方推荐长边 ≤ 1568 像素以达到最佳效果，
/// 这里只在硬上限处拦截，把"推荐值"放在错误提示里告知用户。
pub const KIRO_MAX_IMAGE_SIDE: u32 = 8000;

/// 解析 base64 图片头部，返回 (width, height)
///
/// PNG：解码前 24 字节，IHDR 在 byte 16-23（大端宽 + 大端高）。
/// JPEG：解码前 ~64 字节扫 SOFn 标记。
/// 其它格式或解析失败返回 None，调用方按"通过"处理（避免误杀）。
pub fn image_dimensions(b64_data: &str, format: &str) -> Option<(u32, u32)> {
    let head_chars: String = b64_data.chars().take(96).collect();
    let head = base64::engine::general_purpose::STANDARD
        .decode(head_chars)
        .ok()?;

    match format {
        "png" => {
            // PNG: 8-byte signature + IHDR (4 length + 4 type + 4 width + 4 height + ...)
            if head.len() < 24 {
                return None;
            }
            const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
            if head[..8] != PNG_SIG {
                return None;
            }
            let w = u32::from_be_bytes(head[16..20].try_into().ok()?);
            let h = u32::from_be_bytes(head[20..24].try_into().ok()?);
            Some((w, h))
        }
        "jpeg" => {
            // JPEG 起始 FFD8FF，逐段跳过到首个 SOFn (FFC0..FFCF, 排除 C4/C8/CC)
            if head.len() < 4 || head[0] != 0xff || head[1] != 0xd8 {
                return None;
            }
            let mut i = 2;
            while i + 9 < head.len() {
                if head[i] != 0xff {
                    return None;
                }
                let marker = head[i + 1];
                // SOFn: C0..CF 排除 C4/C8/CC
                if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                    // FF Cn LL LL PP HH HH WW WW
                    if i + 9 >= head.len() {
                        return None;
                    }
                    let h = u16::from_be_bytes([head[i + 5], head[i + 6]]) as u32;
                    let w = u16::from_be_bytes([head[i + 7], head[i + 8]]) as u32;
                    return Some((w, h));
                }
                // segment length 在 marker 之后 2 字节（含自身）
                let seg_len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
                i += 2 + seg_len;
            }
            None
        }
        _ => None,
    }
}

/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 54 prefix + 1 underscore + 8 hash = 63
    let prefix_max = TOOL_NAME_MAX_LEN - 1 - 8;
    let prefix = match name.char_indices().nth(prefix_max) {
        Some((idx, _)) => &name[..idx],
        None => name,
    };
    format!("{}_{}", prefix, hash_suffix)
}

/// 如果名称超长则缩短，并记录映射（short → original）
fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    let short = shorten_tool_name(name);
    tool_name_map.insert(short.clone(), name.to_string());
    short
}

/// 规范化 tool_use_id：净化非法字符 + 缩短超长（确定性，保证 tool_use 和 tool_result 端映射一致）
///
/// 1. 先把 `[a-zA-Z0-9_-]` 以外的字符替换为 `_`，满足 Bedrock 的
///    `^[a-zA-Z0-9_-]+$` 校验（否则 400 `tool_use.id: String should match pattern ...`）。
/// 2. 净化后若仍超长（或净化为空），用原始 id 的 SHA256 前缀生成 `toolu_h` + 24 位 hex
///    （共 31 字符），形态与 Anthropic 原生 `toolu_xxx` 兼容，避免上游 Smithy 长度校验失败。
///
/// tool_use 与 tool_result 两端都以相同的原始 id 调用本函数，因此映射结果一致，不会破坏配对。
fn map_tool_use_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if !sanitized.is_empty() && sanitized.len() <= TOOL_USE_ID_MAX_LEN {
        return sanitized;
    }
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    format!("toolu_h{}", &hash_hex[..24])
}

/// 将 kiro 返回的 tool_use_id 规范化为 Anthropic 客户端兼容形式（出口统一补 `toolu_` 前缀）。
///
/// kiro/CodeWhisperer 返回的 id 形如 `tooluse_xxx`，不带 Anthropic 习惯的 `toolu_`
/// 前缀。部分客户端（如 Claude Code）会自行补 `toolu_` 前缀后存入 tool_result，导致
/// 下一轮请求里 assistant 的 tool_use 与 user 的 tool_result id 前缀不一致 → 配对失败
/// → 上游 400 `TOOL_USE_RESULT_MISMATCH`。这里在发给客户端的出口统一补前缀，使
/// assistant tool_use 与回传的 tool_result id 一致，从源头消除前缀分歧。
pub fn normalize_tool_use_id_for_client(id: &str) -> String {
    if id.starts_with("toolu_") {
        id.to_string()
    } else {
        format!("toolu_{}", id)
    }
}

/// 转换工具定义
fn convert_tools(tools: &Option<Vec<super::types::Tool>>, tool_name_map: &mut HashMap<String, String>) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };

    // 过滤掉 name 为空的工具：客户端无法调用空名工具，且上游 Kiro 对工具名做
    // Smithy @length(min:1) 校验，整条请求会被 400 拒绝。
    let (valid, invalid): (Vec<_>, Vec<_>) = tools
        .iter()
        .partition(|t| !t.name.trim().is_empty());
    if !invalid.is_empty() {
        tracing::warn!(
            dropped = invalid.len(),
            total = tools.len(),
            "过滤掉 name 为空的工具定义，避免上游 400 Improperly formed request"
        );
    }

    valid
        .into_iter()
        .map(|t| {
            let mut description = t.description.clone();

            // 对 Write/Edit 工具追加自定义描述后缀
            let suffix = match t.name.as_str() {
                "Write" => WRITE_TOOL_DESCRIPTION_SUFFIX,
                "Edit" => EDIT_TOOL_DESCRIPTION_SUFFIX,
                _ => "",
            };
            if !suffix.is_empty() {
                description.push('\n');
                description.push_str(suffix);
            }

            // kiro API 不接受空描述（Smithy @length(min:1)，实测 2026-06-18 仍 400），填充工具名作为占位符
            let description = if description.trim().is_empty() {
                t.name.clone()
            } else {
                description
            };

            // 限制描述长度为 10000 字符（安全截断 UTF-8，单次遍历）
            let description = match description.char_indices().nth(10000) {
                Some((idx, _)) => description[..idx].to_string(),
                None => description,
            };

            Tool {
                tool_specification: ToolSpecification {
                    name: map_tool_name(&t.name, tool_name_map),
                    description,
                    input_schema: InputSchema::from_json(normalize_json_schema(serde_json::json!(t.input_schema))),
                },
            }
        })
        .collect()
}

/// 构建顶层 `additionalModelRequestFields`，对齐真实 Kiro IDE 的 thinking 下发方式
///
/// 真实 Kiro IDE 通过 Bedrock 原生 reasoning 字段（请求体顶层
/// `additionalModelRequestFields.thinking`）开启思考，而非把 `<thinking_mode>` 标签
/// 注入 system 文本。仅当请求开启 thinking 时返回 `Some`。
///
/// - `adaptive`：抓包确认形如 `{"thinking":{"type":"adaptive","display":"summarized"},
///   "output_config":{"effort":<effort>}}`。
/// - `enabled`：采用 Bedrock 原生形状 `{"thinking":{"type":"enabled",
///   "budget_tokens":<n>}}`（暂无对应抓包，待实测校正）。
/// 该（已归一化的）模型是否支持 `additionalModelRequestFields.thinking`
///
/// 新 Kiro runtime 端点实测：仅 4.6 及以上世代支持（opus 4.6/4.7/4.8、sonnet 4.6）；
/// 对 opus-4.5 / sonnet-4.5 下发该字段会以 `additionalModelRequestFields is not supported
/// for this model` 400；haiku 不在新端点提供。不支持的模型直接不发该字段（thinking 静默
/// 降级为普通响应），避免带 thinking 的请求整体失败。
fn model_supports_thinking(model: &str) -> bool {
    matches!(
        map_model(model).as_deref(),
        Some("claude-opus-4.6")
            | Some("claude-opus-4.7")
            | Some("claude-opus-4.8")
            | Some("claude-sonnet-4.6")
    )
}

fn build_additional_model_request_fields(req: &MessagesRequest) -> Option<serde_json::Value> {
    let t = req.thinking.as_ref()?;
    // 新 Kiro runtime 端点实测：`thinking.type` 仅接受枚举 ["adaptive","disabled"]，
    // 传 "enabled" 会被上游以 REQUEST_BODY_INVALID 400。故客户端的 enabled（标准 Anthropic
    // 写法）与 adaptive 一律映射为 adaptive；budget_tokens 在该端点无对应字段，忽略。
    if !t.is_enabled() {
        return None;
    }
    // 模型不支持 thinking 时不下发该字段（否则上游 400）。
    if !model_supports_thinking(&req.model) {
        return None;
    }
    let effort = req
        .output_config
        .as_ref()
        .map(|c| c.effort.as_str())
        .unwrap_or("high");
    Some(serde_json::json!({
        "thinking": { "type": "adaptive", "display": "summarized" },
        "output_config": { "effort": effort },
    }))
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 消息切片；最后一条作为 currentMessage（可能是 user，也可能是
///   末尾 assistant(prefill 透传)），其余进入历史。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(req: &MessagesRequest, messages: &[super::types::Message], model_id: &str, tool_name_map: &mut HashMap<String, String>) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 1. 处理系统消息
    if let Some(ref system) = req.system {
        let system_content: String = system
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");

        if !system_content.is_empty() {
            // 追加分块写入策略到系统消息
            let final_content = format!("{}\n{}", system_content, SYSTEM_CHUNKED_POLICY);

            // 系统消息作为 user + assistant 配对
            let user_msg = HistoryUserMessage::new(final_content, model_id);
            history.push(Message::User(user_msg));

            let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
            history.push(Message::Assistant(assistant_msg));
        }
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史（末尾可能是 user 或 assistant）
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();

    for i in 0..history_end_index {
        let msg = &messages[i];

        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
///
/// 历史轮次的图片不再透传到上游：上游对单次请求体有体量上限
/// （AWS CodeWhisperer ~4MB Smithy 校验，超过会以 "Improperly formed
/// request" 形式 400），多轮带图很容易撑爆。模型对历史图的注意力本来就低，
/// 这里只保留一句占位文本告知图片存在，节省 payload。
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut omitted_images = 0usize;
    let mut omitted_documents = 0usize;
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, documents, tool_results) = process_message_content(&msg.content)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        omitted_images += images.len();
        omitted_documents += documents.len();
        all_tool_results.extend(tool_results);
    }

    if omitted_documents > 0 {
        content_parts.insert(
            0,
            format!("[{} document(s) omitted from history]", omitted_documents),
        );
    }
    if omitted_images > 0 {
        content_parts.insert(
            0,
            format!("[{} image(s) omitted from history]", omitted_images),
        );
    }

    // 空 content 实测新端点已接受（Smithy @length(min:1) 已解除），不再用占位符兜底。
    let content = content_parts.join("\n");
    let user_msg = UserMessage::new(&content, model_id);

    let user_msg = if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg.with_context(ctx)
    } else {
        user_msg
    };

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut thinking_content = String::new();
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            if let Some(thinking) = block.thinking {
                                thinking_content.push_str(&thinking);
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                // Kiro 要求 input 必须是 JSON object；客户端偶发传 ""/null/字符串化 JSON，
                                // 非 object 一律归一为 {}，避免上游 REQUEST_BODY_INVALID
                                let input = match block.input {
                                    Some(v @ serde_json::Value::Object(_)) => v,
                                    _ => serde_json::json!({}),
                                };
                                let mapped_name = map_tool_name(&name, tool_name_map);
                                let mapped_id = map_tool_use_id(&id);
                                tool_uses.push(ToolUseEntry::new(mapped_id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    // 组合 thinking 和 text 内容
    // 格式: <thinking>思考内容</thinking>\n\ntext内容
    // 注意: Kiro API 要求 content 字段不能为空，当只有 tool_use 时需要占位符
    let final_content = if !thinking_content.is_empty() {
        if !text_content.is_empty() {
            format!(
                "<thinking>{}</thinking>\n\n{}",
                thinking_content, text_content
            )
        } else {
            format!("<thinking>{}</thinking>", thinking_content)
        }
    } else if text_content.is_empty() && !tool_uses.is_empty() {
        " ".to_string()
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = if content_parts.is_empty() && !all_tool_uses.is_empty() {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod pairing_tests {
    use super::*;
    use crate::kiro::model::requests::tool::{ToolResult, ToolUseEntry};

    /// 构造一个带 tool_results 的历史 user 轮
    fn user_with_results(ids: &[&str]) -> Message {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(
            ids.iter()
                .map(|id| ToolResult::success(*id, "ok".to_string()))
                .collect(),
        );
        Message::User(HistoryUserMessage {
            user_input_message: UserMessage::new(" ", "claude-sonnet-4.5").with_context(ctx),
        })
    }

    /// 构造一个带 tool_uses 的历史 assistant 轮
    fn assistant_with_uses(ids: &[&str]) -> Message {
        let mut a = AssistantMessage::new(" ");
        if !ids.is_empty() {
            a = a.with_tool_uses(
                ids.iter()
                    .map(|id| ToolUseEntry::new(*id, "Read"))
                    .collect(),
            );
        }
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: a,
        })
    }

    fn result_ids(msg: &Message) -> Vec<String> {
        match msg {
            Message::User(u) => u
                .user_input_message
                .user_input_message_context
                .tool_results
                .iter()
                .map(|r| r.tool_use_id.clone())
                .collect(),
            _ => vec![],
        }
    }

    fn use_count(msg: &Message) -> usize {
        match msg {
            Message::Assistant(a) => a
                .assistant_response_message
                .tool_uses
                .as_ref()
                .map(|t| t.len())
                .unwrap_or(0),
            _ => 0,
        }
    }

    #[test]
    fn test_normalize_tool_use_id_adds_prefix() {
        assert_eq!(
            normalize_tool_use_id_for_client("tooluse_Gg6F7Mb"),
            "toolu_tooluse_Gg6F7Mb"
        );
    }

    #[test]
    fn test_normalize_tool_use_id_passthrough() {
        // 已带 toolu_ 前缀的原样返回，避免重复加前缀
        assert_eq!(
            normalize_tool_use_id_for_client("toolu_01ABC"),
            "toolu_01ABC"
        );
        assert_eq!(
            normalize_tool_use_id_for_client("toolu_tooluse_x"),
            "toolu_tooluse_x"
        );
    }

    #[test]
    fn test_map_tool_use_id_sanitizes_illegal_chars() {
        // 含非法字符（`.` `:` `/`）→ 全部替换为 `_`，满足 ^[a-zA-Z0-9_-]+$
        assert_eq!(map_tool_use_id("call.foo:bar/baz"), "call_foo_bar_baz");
        // 合法字符（字母数字 + `_` + `-`）原样保留
        assert_eq!(map_tool_use_id("toolu_01-AbZ9"), "toolu_01-AbZ9");
        // tool_use 与 tool_result 两端以相同原始 id 调用 → 映射一致，配对不破裂
        assert_eq!(map_tool_use_id("x@y"), map_tool_use_id("x@y"));
    }

    #[test]
    fn test_map_tool_use_id_overlong_falls_back_to_hash() {
        let long = format!("call.{}", "x".repeat(80));
        let mapped = map_tool_use_id(&long);
        assert!(mapped.starts_with("toolu_h"));
        assert_eq!(mapped.len(), 31);
        // 确定性：同一输入两次结果相同
        assert_eq!(mapped, map_tool_use_id(&long));
        // 结果本身也满足合法字符集
        assert!(mapped.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    #[test]
    fn test_normalize_json_schema_forces_object_type() {
        // 顶层 type 是非 object 值 → 强制覆盖为 "object"
        let out = normalize_json_schema(serde_json::json!({
            "type": "string",
            "properties": {}
        }));
        assert_eq!(out["type"], serde_json::json!("object"));

        // 缺失 type → 补 "object"
        let out2 = normalize_json_schema(serde_json::json!({"properties": {}}));
        assert_eq!(out2["type"], serde_json::json!("object"));

        // 非 object 顶层（数组）→ 回退为标准空 object schema
        let out3 = normalize_json_schema(serde_json::json!([1, 2, 3]));
        assert_eq!(out3["type"], serde_json::json!("object"));
    }

    #[test]
    fn test_align_removes_orphan_results_after_useless_assistant() {
        // 复现日志现场：assistant 无 tool_use，紧邻的 user 却带 2 个 tool_result
        let mut history = vec![
            assistant_with_uses(&[]),
            user_with_results(&["toolu_tooluse_a", "toolu_tooluse_b"]),
        ];
        align_history_tool_pairing(&mut history);
        // 上一轮 assistant 无 use → 交集为空 → user 的 result 全部清除
        assert_eq!(result_ids(&history[1]).len(), 0);
    }

    #[test]
    fn test_align_keeps_matching_pair() {
        let mut history = vec![
            assistant_with_uses(&["toolu_1", "toolu_2"]),
            user_with_results(&["toolu_1", "toolu_2"]),
        ];
        align_history_tool_pairing(&mut history);
        assert_eq!(use_count(&history[0]), 2);
        assert_eq!(result_ids(&history[1]).len(), 2);
    }

    #[test]
    fn test_align_intersects_partial_overlap() {
        // assistant 有 use {1,2}，user 有 result {2,3} → 只保留交集 {2}
        let mut history = vec![
            assistant_with_uses(&["toolu_1", "toolu_2"]),
            user_with_results(&["toolu_2", "toolu_3"]),
        ];
        align_history_tool_pairing(&mut history);
        assert_eq!(use_count(&history[0]), 1);
        assert_eq!(result_ids(&history[1]), vec!["toolu_2".to_string()]);
    }

    #[test]
    fn test_align_dedups_duplicate_results() {
        let mut history = vec![
            assistant_with_uses(&["toolu_1"]),
            user_with_results(&["toolu_1", "toolu_1"]),
        ];
        align_history_tool_pairing(&mut history);
        assert_eq!(result_ids(&history[1]), vec!["toolu_1".to_string()]);
    }

    #[test]
    fn test_convert_request_drops_current_orphan_result() {
        // 复现 17:13 dump：currentMessage 带一个在 history 中找不到对应 tool_use 的
        // orphan toolResult（call_wfsHLid），且其前一轮 assistant 是自动补的 "OK"。
        // 期望：current 的 orphan result 被 validate_tool_pairing 丢弃，不发往上游。
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 64,
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("search ankr login api"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "Let me search."},
                        {"type": "tool_use", "id": "call_1d6", "name": "Glob", "input": {"pattern": "**/*.ts"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "call_1d6", "content": "No file found"}
                    ]),
                },
                // 最后一条 = currentMessage：orphan toolResult，无对应 tool_use
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "call_wfsHLid", "content": "Nothing found"}
                    ]),
                },
            ],
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        let current_results = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results;
        assert!(
            current_results.is_empty(),
            "current 的 orphan toolResult 应被丢弃，实际保留了 {} 个",
            current_results.len()
        );

        // 同时：history 中也不应残留 call_wfsHLid 的 result
        for msg in &result.conversation_state.history {
            if let Message::User(u) = msg {
                for r in &u.user_input_message.user_input_message_context.tool_results {
                    assert_ne!(r.tool_use_id, "call_wfsHLid", "history 不应残留 orphan result");
                }
            }
        }
    }

    #[test]
    fn test_convert_request_multi_tool_split_results() {
        // 复现 17:13 live bug：assistant 一轮调用两个工具（call_1d6 + call_wfsHLid），
        // call_1d6 的结果在 history、call_wfsHLid 的结果在 currentMessage。
        // 旧顺序（validate 先于 align）会保留 current 的 call_wfsHLid result，再被 align
        // 删掉对应 history use，导致 orphan → 上游 400。
        // 修复后（align 先于 validate）：align 移除 history 中 call_wfsHLid use，validate
        // 随即把 current 的 call_wfsHLid orphan result 丢弃 → 请求合法。
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-opus-4-7".to_string(),
            max_tokens: 64,
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("search ankr login api"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "Let me search."},
                        {"type": "tool_use", "id": "call_1d6", "name": "Glob", "input": {}},
                        {"type": "tool_use", "id": "call_wfsHLid", "name": "SearchCodebase", "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "call_1d6", "content": "No file found"}
                    ]),
                },
                // currentMessage：call_wfsHLid 的结果
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "call_wfsHLid", "content": "Nothing found"}
                    ]),
                },
            ],
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        let cs = &result.conversation_state;

        // 收集 history 中所有 tool_use_id
        let history_use_ids: std::collections::HashSet<String> = cs
            .history
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => a.assistant_response_message.tool_uses.as_ref(),
                _ => None,
            })
            .flatten()
            .map(|t| t.tool_use_id.clone())
            .collect();

        // current 的每个 result 都必须能在 history 中找到对应 use（否则就是会触发 400 的 orphan）
        for r in &cs.current_message.user_input_message.user_input_message_context.tool_results {
            assert!(
                history_use_ids.contains(&r.tool_use_id),
                "current result {} 在 history 中无对应 tool_use（orphan，会触发 400）",
                r.tool_use_id
            );
        }

        // 逐轮校验 history：每个 user 轮的 result 数 <= 紧邻上一轮 assistant 的 use 数
        for i in 1..cs.history.len() {
            if let Message::User(u) = &cs.history[i] {
                let res = u.user_input_message.user_input_message_context.tool_results.len();
                let prev_uses = match &cs.history[i - 1] {
                    Message::Assistant(a) => a
                        .assistant_response_message
                        .tool_uses
                        .as_ref()
                        .map(|t| t.len())
                        .unwrap_or(0),
                    _ => 0,
                };
                assert!(res <= prev_uses, "history[{}] result 数 {} > 上一轮 use 数 {}", i, res, prev_uses);
            }
        }
    }

    #[test]
    fn test_align_skips_trailing_assistant() {
        // history 末尾的 assistant（其 result 在 currentMessage）不应被裁剪
        let mut history = vec![
            user_with_results(&[]),
            assistant_with_uses(&["toolu_pending"]),
        ];
        align_history_tool_pairing(&mut history);
        assert_eq!(use_count(&history[1]), 1, "尾部 assistant 的 tool_use 应保留");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_model_sonnet() {
        assert!(
            map_model("claude-sonnet-4-20250514")
                .unwrap()
                .contains("sonnet")
        );
        assert!(
            map_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .contains("sonnet")
        );
    }

    #[test]
    fn test_map_model_opus() {
        assert!(
            map_model("claude-opus-4-20250514")
                .unwrap()
                .contains("opus")
        );
    }

    #[test]
    fn test_map_model_haiku() {
        assert!(
            map_model("claude-haiku-4-20250514")
                .unwrap()
                .contains("haiku")
        );
    }

    #[test]
    fn test_map_model_unsupported() {
        assert!(map_model("gpt-4").is_none());
    }

    #[test]
    fn test_map_model_thinking_suffix_sonnet() {
        // thinking 后缀不应影响 sonnet 模型映射
        let result = map_model("claude-sonnet-4-5-20250929-thinking");
        assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_5() {
        // thinking 后缀不应影响 opus 4.5 模型映射
        let result = map_model("claude-opus-4-5-20251101-thinking");
        assert_eq!(result, Some("claude-opus-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_6() {
        // thinking 后缀不应影响 opus 4.6 模型映射
        let result = map_model("claude-opus-4-6-thinking");
        assert_eq!(result, Some("claude-opus-4.6".to_string()));
    }

    #[test]
    fn test_map_model_opus_4_7() {
        // 4.7 透传上游 id，不兜底到 4.6
        assert_eq!(
            map_model("claude-opus-4-7"),
            Some("claude-opus-4.7".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4-7-thinking"),
            Some("claude-opus-4.7".to_string())
        );
    }

    #[test]
    fn test_map_model_opus_4_8() {
        // 4.8 透传上游 id，不兜底到 4.6
        assert_eq!(
            map_model("claude-opus-4-8"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4-8-thinking"),
            Some("claude-opus-4.8".to_string())
        );
    }

    #[test]
    fn test_context_window_opus_4_7_4_8() {
        assert_eq!(get_context_window_size("claude-opus-4-7"), 1_000_000);
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_fable_5() {
        // fable 系列兜底到上游 claude-opus-4.8（上游暂无 fable，实测 opus-4.8 可用）
        assert_eq!(
            map_model("claude-fable-5"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-fable-5-thinking"),
            Some("claude-opus-4.8".to_string())
        );
        // 兜底到 opus-4.8 后享受 1M 上下文窗口与 opus 价档
        assert_eq!(get_context_window_size("claude-fable-5"), 1_000_000);
    }

    /// 官方价折算：对齐 Anthropic 文档的 worked example
    /// （Opus 4.8：10k uncached + 40k cache 读 + 15k output，仅 token 部分 = $0.445）。
    #[test]
    fn official_price_matches_anthropic_worked_example() {
        // 0.05(uncached) + 0.02(cache读) + 0.375(output) = 0.445
        let p = official_price_usd("claude-opus-4-8", 10_000, 40_000, 0, 0, 15_000);
        assert!((p - 0.445).abs() < 1e-9, "got {p}");
    }

    /// 各模型档位与 cache 倍率正确（5m=1.25×、1h=2×、读=0.1×）。
    #[test]
    fn official_price_tiers_and_cache_multipliers() {
        // Sonnet-4.6 input $3：100 万 uncached input = $3。
        assert!((official_price_usd("claude-sonnet-4-6", 1_000_000, 0, 0, 0, 0) - 3.0).abs() < 1e-9);
        // Opus 5m 写 = 1.25 × $5 = $6.25 / MTok。
        assert!((official_price_usd("claude-opus-4-8", 0, 0, 1_000_000, 0, 0) - 6.25).abs() < 1e-9);
        // Opus 1h 写 = 2 × $5 = $10 / MTok。
        assert!((official_price_usd("claude-opus-4-8", 0, 0, 0, 1_000_000, 0) - 10.0).abs() < 1e-9);
        // Haiku cache 读 = 0.1 × $1 = $0.10 / MTok。
        assert!((official_price_usd("claude-haiku-4-5", 0, 1_000_000, 0, 0, 0) - 0.10).abs() < 1e-9);
    }

    /// 动态窗口表存在时优先用上游 maxInputTokens（按 map_model 归一化匹配），
    /// 缺失/为 0 时回退硬编码常量。用纯函数 window_size_for 测，避免全局态 flaky。
    #[test]
    fn dynamic_window_overrides_then_falls_back() {
        let mut dynamic = HashMap::new();
        dynamic.insert("claude-opus-4.8".to_string(), 700_000); // 上游真实窗口
        dynamic.insert("claude-sonnet-4.6".to_string(), 0); // 非法值应被忽略

        // 命中动态值（客户端命名 opus-4-8，map_model 归一化为 4.8 匹配）。
        assert_eq!(window_size_for("claude-opus-4-8", &dynamic), 700_000);
        // 动态值非法（0）→ 回退硬编码 1M。
        assert_eq!(window_size_for("claude-sonnet-4-6", &dynamic), 1_000_000);
        // 动态表无此模型 → 回退硬编码 200K。
        assert_eq!(window_size_for("claude-opus-4-5", &dynamic), 200_000);
        // 空表 → 全部回退硬编码（opus 4.8 硬编码 1M）。
        let empty = HashMap::new();
        assert_eq!(window_size_for("claude-opus-4-8", &empty), 1_000_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        // thinking 后缀不应影响 haiku 模型映射
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, Some("claude-haiku-4.5".to_string()));
    }

    /// 构建一个最小可用的 MessagesRequest，仅关心 thinking / output_config 字段
    fn req_with_thinking(
        thinking: Option<super::super::types::Thinking>,
        effort: Option<&str>,
    ) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking,
            output_config: effort.map(|e| super::super::types::OutputConfig { effort: e.to_string() }),
            metadata: None,
            messages: vec![],
        }
    }

    #[test]
    fn test_additional_model_request_fields_adaptive() {
        // adaptive：对齐真实 Kiro 抓包，display=summarized + output_config.effort
        let req = req_with_thinking(
            Some(super::super::types::Thinking { thinking_type: "adaptive".to_string(), budget_tokens: 20000 }),
            Some("high"),
        );
        let fields = build_additional_model_request_fields(&req).unwrap();
        assert_eq!(
            fields,
            serde_json::json!({
                "thinking": { "type": "adaptive", "display": "summarized" },
                "output_config": { "effort": "high" },
            })
        );
    }

    #[test]
    fn test_additional_model_request_fields_adaptive_default_effort() {
        // adaptive 无 output_config 时 effort 回退 high
        let req = req_with_thinking(
            Some(super::super::types::Thinking { thinking_type: "adaptive".to_string(), budget_tokens: 20000 }),
            None,
        );
        let fields = build_additional_model_request_fields(&req).unwrap();
        assert_eq!(fields["output_config"]["effort"], "high");
    }

    #[test]
    fn test_additional_model_request_fields_enabled_maps_to_adaptive() {
        // 上游只认 ["adaptive","disabled"]：客户端 enabled 必须映射成 adaptive
        let req = req_with_thinking(
            Some(super::super::types::Thinking { thinking_type: "enabled".to_string(), budget_tokens: 12000 }),
            None,
        );
        let fields = build_additional_model_request_fields(&req).unwrap();
        assert_eq!(
            fields,
            serde_json::json!({
                "thinking": { "type": "adaptive", "display": "summarized" },
                "output_config": { "effort": "high" },
            })
        );
    }

    #[test]
    fn test_additional_model_request_fields_none_when_no_thinking() {
        let req = req_with_thinking(None, None);
        assert!(build_additional_model_request_fields(&req).is_none());
    }

    #[test]
    fn test_additional_model_request_fields_gated_by_model() {
        let thinking =
            || Some(super::super::types::Thinking { thinking_type: "adaptive".to_string(), budget_tokens: 0 });

        // 支持 thinking 的世代：发字段
        for m in [
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-fable-5", // fable → opus-4.8
        ] {
            let mut req = req_with_thinking(thinking(), Some("high"));
            req.model = m.to_string();
            assert!(
                build_additional_model_request_fields(&req).is_some(),
                "{m} 应下发 additionalModelRequestFields"
            );
        }

        // 不支持的模型：不发字段（避免上游 400）
        for m in [
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
        ] {
            let mut req = req_with_thinking(thinking(), Some("high"));
            req.model = m.to_string();
            assert!(
                build_additional_model_request_fields(&req).is_none(),
                "{m} 不应下发 additionalModelRequestFields"
            );
        }
    }

    #[test]
    fn test_process_message_content_document_pdf() {
        let content = serde_json::json!([
            {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0x"},"title":"sub/report.pdf"},
            {"type":"text","text":"读它"}
        ]);
        let (text, images, docs, tools) = process_message_content(&content).unwrap();
        assert_eq!(text, "读它");
        assert!(images.is_empty());
        assert!(tools.is_empty());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "report"); // 去路径与扩展名
        assert_eq!(docs[0].format, "pdf");
        assert_eq!(docs[0].source.bytes, "JVBERi0x");
    }

    #[test]
    fn test_process_message_content_document_defaults_and_skips() {
        // 无 title → 默认名 "document"
        let pdf = serde_json::json!([
            {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AAAA"}}
        ]);
        let (_t, _i, docs, _r) = process_message_content(&pdf).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "document");

        // 未知 media_type → 跳过
        let zip = serde_json::json!([
            {"type":"document","source":{"type":"base64","media_type":"application/zip","data":"AAAA"}}
        ]);
        let (_t, _i, docs, _r) = process_message_content(&zip).unwrap();
        assert!(docs.is_empty());

        // 非 base64 源 → 跳过
        let urlsrc = serde_json::json!([
            {"type":"document","source":{"type":"url","media_type":"application/pdf","data":"http://x/y.pdf"}}
        ]);
        let (_t, _i, docs, _r) = process_message_content(&urlsrc).unwrap();
        assert!(docs.is_empty());
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name = "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(short1.len() <= TOOL_NAME_MAX_LEN, "短名称长度应 <= 63，实际 {}", short1.len());
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("test"),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result.conversation_state.current_message.user_input_message
            .user_input_message_context.tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_convert_tools_filters_empty_name() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        // 模拟 OpenClaw 客户端发来的脏数据：16 个空 name 工具混在 1 个正常工具中
        let mut tools: Vec<AnthropicTool> = (0..16)
            .map(|_| AnthropicTool {
                name: String::new(),
                description: String::new(),
                input_schema: schema.clone(),
                tool_type: None,
                max_uses: None,
                cache_control: None,
            })
            .collect();
        tools.push(AnthropicTool {
            name: "valid_tool".to_string(),
            description: "ok".to_string(),
            input_schema: schema.clone(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        });
        // 一个 name 只包含空白字符的也要被过滤
        tools.push(AnthropicTool {
            name: "   ".to_string(),
            description: "blank".to_string(),
            input_schema: schema.clone(),
            tool_type: None,
            max_uses: None,
            cache_control: None,
        });

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            stream: false,
            tools: Some(tools),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        let kiro_tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert_eq!(kiro_tools.len(), 1, "只保留 1 个 name 非空的工具");
        assert_eq!(kiro_tools[0].tool_specification.name, "valid_tool");
    }

    #[test]
    fn test_history_images_dropped_with_placeholder() {
        use super::super::types::Message as AnthropicMessage;

        // 历史里一条 user 消息带 2 张图 + 一行文字；当前消息是纯文本
        let tiny_b64 = "iVBORw0KGgo="; // 小占位
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 64,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "look at these"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": tiny_b64}},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": tiny_b64}}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!("seen"),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("follow up"),
                },
            ],
            system: None,
            stream: false,
            tools: None,
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();

        // 历史里第一条 user 应该不含图，但 content 头部应有占位符
        let history = &result.conversation_state.history;
        let first = match &history[0] {
            Message::User(h) => h,
            _ => panic!("expected User"),
        };
        assert!(first.user_input_message.images.is_empty(), "历史图片应被丢弃");
        assert!(
            first
                .user_input_message
                .content
                .contains("[2 image(s) omitted from history]"),
            "应在历史文本里追加占位符，实际: {:?}",
            first.user_input_message.content
        );
        // 原始文字也得保留
        assert!(first.user_input_message.content.contains("look at these"));
    }

    #[test]
    fn test_image_dimensions_png_parses_width_height() {
        // 手工构造 PNG 头：sig + IHDR(13) + "IHDR" + width(BE) + height(BE) + 5 dummy
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&8619u32.to_be_bytes()); // width
        bytes.extend_from_slice(&5315u32.to_be_bytes()); // height
        bytes.extend_from_slice(&[0, 0, 0, 0, 0]); // depth/color/etc
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let dims = image_dimensions(&b64, "png");
        assert_eq!(dims, Some((8619, 5315)));
    }

    #[test]
    fn test_image_dimensions_non_png_returns_none() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not a png");
        assert_eq!(image_dimensions(&b64, "png"), None);
        assert_eq!(image_dimensions(&b64, "webp"), None);
    }

    #[test]
    fn test_convert_request_rejects_oversize_current_image() {
        use super::super::types::Message as AnthropicMessage;

        // 构造 8619×5315 的 PNG 头（复刻日志里的图）
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&8619u32.to_be_bytes());
        bytes.extend_from_slice(&5315u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 16,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type":"base64","media_type":"image/png","data": b64}}
                ]),
            }],
            system: None,
            stream: false,
            tools: None,
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
        };

        let err = convert_request(&req, "AI_EDITOR", false).unwrap_err();
        match err {
            ConversionError::ImageTooLarge { width, height, max_side } => {
                assert_eq!(width, 8619);
                assert_eq!(height, 5315);
                assert_eq!(max_side, KIRO_MAX_IMAGE_SIDE);
            }
            other => panic!("expected ImageTooLarge, got {:?}", other),
        }
    }

    #[test]
    fn test_kiro_max_image_side_within_aws_documented_limit() {
        assert!(KIRO_MAX_IMAGE_SIDE <= 8000);
        assert!(KIRO_MAX_IMAGE_SIDE >= 1568);
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 测试没有 metadata 的请求，应该生成新的 UUID
        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_remove_orphaned_tool_uses() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试从历史中移除孤立的 tool_use
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 移除 tool-1 和 tool-3
        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证只剩下 tool-2
        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].tool_use_id, "tool-2");
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_remove_orphaned_tool_uses_all_removed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试移除所有 tool_use 后，tool_uses 变为 None
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证 tool_uses 变为 None
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(
                assistant_msg.assistant_response_message.tool_uses.is_none(),
                "移除所有 tool_use 后应为 None"
            );
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(&messages, &mut HashMap::new()).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(content.contains("<thinking>"), "应包含 thinking 标签");
        assert!(content.contains("Let me read that file"), "应包含第二条消息的 text 内容");

        let tool_uses = result.assistant_response_message.tool_uses.expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let result = convert_request(&req, "AI_EDITOR", false);
        assert!(result.is_ok(), "连续 assistant 消息场景不应报错: {:?}", result.err());

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }
}

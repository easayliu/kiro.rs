//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use uuid::Uuid;

use super::cache_tracker::{CacheTracker, CacheWriteback};
use crate::kiro::model::events::Event;

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        // 如果标签后面内容不足以判断是否有双换行符，等待更多内容
        if after_content.len() < 2 {
            return None;
        }

        // 真正的 thinking 结束标签后面会有双换行符 `\n\n`
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) =
        if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
            (
                &after_open[..end_pos],
                &after_open[end_pos + "</thinking>\n\n".len()..],
            )
        } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
            let after_tag = end_pos + "</thinking>".len();
            (
                &after_open[..end_pos],
                after_open[after_tag..].trim_start(),
            )
        } else {
            // 找不到有效的结束标签，不做提取
            return (None, text.to_string());
        };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw
        .strip_prefix('\n')
        .unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// message_delta/final_usage 中的缓存 token 细分
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheUsage {
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
    /// 最后 breakpoint 之后的未缓存 tokens，对应 Anthropic 返回的 input_tokens
    pub uncached_input_tokens: i32,
}

impl CacheUsage {
    /// 计费口径拆分：在「总量精确」的前提下尽量做到「读写守恒」。
    ///
    /// 与 [`Self::scaled_to_upstream`] 的区别：cache_read **不再独立缩放**，而是钉到
    /// `billed_read`（W：这段前缀上一次被计费时的 read+creation 之和，由命中条目持久化）。
    /// 形式化：给定上游真实总量 `T`，输出 `(r, c, u)` 满足
    /// - 总量精确：`r + c + u == T`（恒成立，c/u 由减法导出）
    /// - 读写守恒：`r == W`（当 `W <= T`，即正常情况）
    ///
    /// 唯一无法同时满足的退化情形是 `W > T`（前缀上次计费量比本次整个上游总量还大，
    /// 只可能源于跨请求计量抖动）：此时 `r = min(W, T)` 截断，保总量精确、守恒尽力而为。
    ///
    /// `billed_read = None`（首次命中 / 前序计费未回写）时退回缩放本地 cache_read。
    /// `estimated_total <= 0` 或 `context_total <= 0` 时原样返回（无可用比例）。
    pub(crate) fn billed_split(
        &self,
        estimated_total: i32,
        context_total: i32,
        billed_read: Option<i32>,
    ) -> Self {
        if estimated_total <= 0 || context_total <= 0 {
            return *self;
        }
        let scale = context_total as f64 / estimated_total as f64;
        let scale_one = |v: i32| ((v as f64 * scale).round() as i32).max(0);

        // r：钉到历史 billed 值（守恒）；无历史值时退回缩放本地估算。封顶到 T。
        let cache_read = billed_read
            .unwrap_or_else(|| scale_one(self.cache_read_input_tokens))
            .clamp(0, context_total);

        // 剩余 R = T - r 是「上游眼里的新增」，按本地 creation/uncached 比例劈分。
        let remainder = context_total - cache_read;
        let local_creation = self.cache_creation_input_tokens.max(0);
        let local_uncached = self.uncached_input_tokens.max(0);
        let split = |part: i32, denom: i32, total: i32| -> i32 {
            if denom <= 0 {
                return 0;
            }
            (((total as f64) * (part as f64) / (denom as f64)).round() as i32).clamp(0, total)
        };
        let cache_creation = split(local_creation, local_creation + local_uncached, remainder);
        // uncached 减法兜底，确保三者之和恰为 context_total。
        let uncached = remainder - cache_creation;

        // creation 内部按本地 5m/1h 比例二次劈分（减法兜底保证 5m + 1h == creation）。
        let local_5m = self.cache_creation_5m_input_tokens.max(0);
        let local_1h = self.cache_creation_1h_input_tokens.max(0);
        let cache_5m = if local_5m + local_1h > 0 {
            split(local_5m, local_5m + local_1h, cache_creation)
        } else {
            // 细分缺失但 creation 存在（理论不会发生）：全部归 5m，与历史兜底一致。
            cache_creation
        };
        let cache_1h = cache_creation - cache_5m;

        Self {
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: cache_5m,
            cache_creation_1h_input_tokens: cache_1h,
            uncached_input_tokens: uncached,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
    /// message_delta/final 阶段透传的缓存使用细分
    final_usage: Option<CacheUsage>,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
            final_usage: None,
        }
    }

    /// 设置最终 usage（含 prompt caching 细分）
    pub fn set_final_usage(&mut self, usage: CacheUsage) {
        self.final_usage = Some(usage);
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            // 上报上游返回的真实 token 计数（无 cache 信息时输入侧仅为 input_tokens）。
            let usage_json = self.final_usage.map_or_else(
                || {
                    json!({
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                    })
                },
                |usage| {
                    json!({
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        "cache_read_input_tokens": usage.cache_read_input_tokens,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": usage.cache_creation_5m_input_tokens,
                            "ephemeral_1h_input_tokens": usage.cache_creation_1h_input_tokens
                        }
                    })
                },
            );
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": usage_json
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::{credit_to_usd, get_context_window_size, official_price_usd};
use super::handlers::CacheUsageContext;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值，用于 contextUsageEvent 的上下文窗口判断）
    #[allow(dead_code)]
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    #[allow(dead_code)]
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// Prompt caching 使用细分（由 handler 在 API 调用返回后注入）
    pub cache_usage: CacheUsage,
    /// 命中条目持久化的「上游计费口径」累计 token（W）。计费时把 cache_read
    /// 钉到该值以保证读写守恒；`None` 时回退到缩放本地估算。
    cache_read_billed: Option<i32>,
    /// 计费完成后的回写句柄：(tracker, writeback)。收到 contextUsageEvent 时，
    /// 在 generate_final_events 里把缩放后的 billed 累计回写到缓存条目。
    billing_writeback: Option<(Arc<CacheTracker>, CacheWriteback)>,
    /// meteringEvent 给出的上游真实扣费（单位 credit），用于成本对账 / 诊断。
    upstream_credit: Option<f64>,
    /// StreamContext 创建时刻，用于在计费汇总日志里输出请求耗时。
    start: Instant,
    /// 上游首字节到达时刻，用于在汇总日志里输出 TTFT（首字耗时）。
    first_byte_at: Option<Instant>,
    /// TTFT 计时原点：向上游发出请求的时刻（由 handler 注入）。流式下上游常等首
    /// token 生成好才 flush 响应头，故以此为原点才能量到真实首字耗时；缺省回退 start。
    ttft_origin: Option<Instant>,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// 是否已收到过 `reasoningContentEvent`（新 Kiro runtime 端点的独立思考流）。
    /// 一旦为 true，说明思考经独立事件下发，`assistantResponseEvent` 的正文即纯文本，
    /// 无需再走 `<thinking>` 文本标签扫描（那是老 CodeWhisperer 端点的内联格式）。
    reasoning_seen: bool,
    /// 是否抓取流式响应内容用于 debug dump（构造时按日志级别决定，info 下为 false 零开销）。
    debug_capture: bool,
    /// debug 抓取：累积的助手正文。
    dbg_text: String,
    /// debug 抓取：累积的思考内容。
    dbg_thinking: String,
    /// debug 抓取：累积的工具调用 (id, name, 部分 input JSON 拼接)。
    dbg_tools: Vec<(String, String, String)>,
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            output_tokens: 0,
            cache_usage: CacheUsage::default(),
            cache_read_billed: None,
            billing_writeback: None,
            upstream_credit: None,
            start: Instant::now(),
            first_byte_at: None,
            ttft_origin: None,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            reasoning_seen: false,
            debug_capture: tracing::enabled!(tracing::Level::DEBUG),
            dbg_text: String::new(),
            dbg_thinking: String::new(),
            dbg_tools: Vec::new(),
        }
    }

    /// 注入 prompt caching 结果
    pub fn set_cache_usage(&mut self, ctx: CacheUsageContext) {
        self.cache_usage = CacheUsage {
            cache_creation_input_tokens: ctx.cache_creation_input_tokens,
            cache_read_input_tokens: ctx.cache_read_input_tokens,
            cache_creation_5m_input_tokens: ctx.cache_creation_5m_input_tokens,
            cache_creation_1h_input_tokens: ctx.cache_creation_1h_input_tokens,
            uncached_input_tokens: ctx.uncached_input_tokens,
        };
        self.cache_read_billed = ctx.cache_read_billed;
    }

    /// 注入计费完成后的回写句柄。收到 contextUsageEvent 时，generate_final_events
    /// 会把缩放后的 billed 累计回写到缓存条目，供下次命中实现读写守恒。
    pub fn set_billing_writeback(&mut self, tracker: Arc<CacheTracker>, writeback: CacheWriteback) {
        self.billing_writeback = Some((tracker, writeback));
    }

    /// 设置 TTFT 计时原点（向上游发出请求的时刻）。
    pub fn set_ttft_origin(&mut self, origin: Instant) {
        self.ttft_origin = Some(origin);
    }

    /// 标记上游首字节到达时刻。仅首次调用生效。
    pub fn mark_first_byte(&mut self) {
        if self.first_byte_at.is_none() {
            self.first_byte_at = Some(Instant::now());
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        let input = self.cache_usage.uncached_input_tokens.max(1);
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": self.cache_usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": self.cache_usage.cache_read_input_tokens,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": self.cache_usage.cache_creation_5m_input_tokens,
                        "ephemeral_1h_input_tokens": self.cache_usage.cache_creation_1h_input_tokens,
                    }
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        // debug 抓取：累积响应内容，供流末尾 dump 一个拼好的最终响应（info 下零开销）。
        if self.debug_capture {
            match event {
                Event::AssistantResponse(resp) => self.dbg_text.push_str(&resp.content),
                Event::ReasoningContent(reasoning) => self.dbg_thinking.push_str(&reasoning.text),
                Event::ToolUse(tu) => {
                    // 工具 input 可能分多帧到达：同一 id（或后续帧 id 为空）则拼接，否则新开一条。
                    let same = self
                        .dbg_tools
                        .last()
                        .map(|(id, _, _)| id == &tu.tool_use_id || tu.tool_use_id.is_empty())
                        .unwrap_or(false);
                    if same {
                        if let Some((_, _, input)) = self.dbg_tools.last_mut() {
                            input.push_str(&tu.input);
                        }
                    } else {
                        self.dbg_tools.push((
                            tu.tool_use_id.clone(),
                            tu.name.clone(),
                            tu.input.clone(),
                        ));
                    }
                }
                _ => {}
            }
        }
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比反推实际 input_tokens。
                // 用 round 而非截断：上游百分比是满精度的，round 能精确还原其真实
                // token 整数（例如 1.8410999774932861% × 1_000_000 = 18410.9998 → 18411）。
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens = (context_usage.context_usage_percentage
                    * (window_size as f64)
                    / 100.0)
                    .round() as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Metering(metering) => {
                self.upstream_credit = Some(metering.usage);
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// debug 级 dump 流式响应体：把累积的思考/正文/工具调用拼成一个最终 Anthropic 消息打印，
    /// 与非流式的 "Anthropic response body" 日志格式对齐。仅在 debug_capture 时有内容。
    pub fn debug_dump_response_body(&self) {
        if !self.debug_capture {
            return;
        }
        let mut content = Vec::new();
        if !self.dbg_thinking.is_empty() {
            content.push(serde_json::json!({"type": "thinking", "thinking": self.dbg_thinking}));
        }
        if !self.dbg_text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": self.dbg_text}));
        }
        for (id, name, input) in &self.dbg_tools {
            // 还原工具原名（短名映射）+ 补客户端 toolu_ 前缀，与实际下发一致。
            let original_name = self.tool_name_map.get(name).cloned().unwrap_or_else(|| name.clone());
            let client_id = super::converter::normalize_tool_use_id_for_client(id);
            // input 是拼接的部分 JSON 字符串，能解析成 JSON 就解析，否则原样作字符串。
            let parsed_input: serde_json::Value =
                serde_json::from_str(input).unwrap_or_else(|_| serde_json::Value::String(input.clone()));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": client_id,
                "name": original_name,
                "input": parsed_input,
            }));
        }
        let body = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": self.model,
            "stop_reason": self.state_manager.get_stop_reason(),
            "output_tokens": self.output_tokens,
        });
        tracing::debug!("Anthropic response body (stream): {}", body);
    }

    /// 处理推理（思考）内容事件（新 Kiro runtime 端点的独立思考流）
    ///
    /// 上游以 `reasoningContentEvent` 逐 token 下发 `{"text":...}`，并在末尾用
    /// `{"signature":...}` 携带思考签名。这里懒启动一个 Anthropic `thinking` 块，
    /// 把文本作为 `thinking_delta`、签名作为 `signature_delta` 流式转发。
    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.reasoning_seen = true;

        // 懒启动 thinking 块
        if self.thinking_block_index.is_none() {
            let thinking_index = self.state_manager.next_block_index();
            self.thinking_block_index = Some(thinking_index);
            self.in_thinking_block = true;
            let start_events = self.state_manager.handle_content_block_start(
                thinking_index,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": thinking_index,
                    "content_block": { "type": "thinking", "thinking": "" }
                }),
            );
            events.extend(start_events);
        }
        let Some(thinking_index) = self.thinking_block_index else {
            return events;
        };

        if !reasoning.text.is_empty() {
            self.output_tokens += estimate_tokens(&reasoning.text);
            events.push(self.create_thinking_delta_event(thinking_index, &reasoning.text));
        }
        if let Some(signature) = &reasoning.signature {
            if !signature.is_empty() {
                events.push(self.create_signature_delta_event(thinking_index, signature));
            }
        }

        events
    }

    /// 关闭由 `reasoningContentEvent` 开启的 thinking 块（发送 content_block_stop）。
    fn close_reasoning_block(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.in_thinking_block {
            if let Some(thinking_index) = self.thinking_block_index {
                if let Some(stop_event) =
                    self.state_manager.handle_content_block_stop(thinking_index)
                {
                    events.push(stop_event);
                }
            }
        }
        self.in_thinking_block = false;
        self.thinking_extracted = true;
        events
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 新端点（思考走独立 reasoningContentEvent）：先收尾思考块，正文按纯文本处理，
        // 不再走 `<thinking>` 文本标签扫描（该格式仅老 CodeWhisperer 端点会内联返回）。
        if self.reasoning_seen {
            let mut events = self.close_reasoning_block();
            if content.is_empty() {
                return events;
            }
            self.output_tokens += estimate_tokens(content);
            events.extend(self.create_text_delta_events(content));
            return events;
        }

        if content.is_empty() {
            return Vec::new();
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            return self.process_content_with_thinking(content);
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        self.create_text_delta_events(content)
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离 `</thinking>\n\n`（find_real_thinking_end_tag 已确认 \n\n 存在）
                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件（思考块签名，对齐 Anthropic thinking 流格式）
    fn create_signature_delta_event(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
                }
            }),
        )
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // 新端点：reasoningContentEvent 开的 thinking 块需在 tool_use 前收尾。
        if self.reasoning_seen && self.in_thinking_block {
            events.extend(self.close_reasoning_block());
        }

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // 再发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 规范化 tool_use_id：补 `toolu_` 前缀，与回传的 tool_result id 对齐
        let client_tool_use_id =
            super::converter::normalize_tool_use_id_for_client(&tool_use.tool_use_id);

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": client_tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token

            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": tool_use.input
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop {
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// 生成最终事件序列
    /// 计费口径的缓存使用量。
    ///
    /// 有 contextUsageEvent 时，把本地估算的缓存拆分按上游实际总量等比缩放，
    /// 使 cache_* 与 uncached 全部落在上游计量体系（见
    /// [`CacheUsage::scaled_to_upstream`]）；否则用 cache_tracker 基于请求估算
    /// 算出的本地拆分。
    fn billing_cache_usage(&self) -> CacheUsage {
        match self.context_input_tokens {
            Some(context_total) => self.cache_usage.billed_split(
                self.input_tokens,
                context_total,
                self.cache_read_billed,
            ),
            None => self.cache_usage,
        }
    }

    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 新端点：若 reasoningContentEvent 开的 thinking 块仍未收尾（无后续正文/工具），
        // 在收口前补一个 content_block_stop。
        if self.reasoning_seen && self.in_thinking_block {
            events.extend(self.close_reasoning_block());
        }

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &self.thinking_buffer),
                        );
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块。
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            // 仅老内联 `<thinking>` 路径：thinking-only 意味着模型把预算耗在思考上（被截断），
            // 标记 max_tokens。新端点 reasoningContentEvent 的思考是独立完整流（带签名收尾），
            // thinking-only 是正常结束，不应改写上游真实 stop_reason。
            if !self.reasoning_seen {
                self.state_manager.set_stop_reason("max_tokens");
            }
            events.extend(self.create_text_delta_events(" "));
        }

        // 计费口径：有 contextUsageEvent 时钉住 cache_read 到历史 billed 值（守恒）、
        // 剩余按本地比例劈给 creation/uncached（保总量精确）；否则用本地估算拆分。
        let billing = self.billing_cache_usage();

        // 计费完成后回写 billed 累计到缓存条目（仅有上游真实总量时），供下次命中守恒。
        if self.context_input_tokens.is_some() {
            if let Some((tracker, writeback)) = &self.billing_writeback {
                tracker.apply_billing_writeback(
                    writeback,
                    billing.cache_read_input_tokens,
                    billing.cache_creation_input_tokens,
                );
            }
        }

        let actual = credit_to_usd(self.upstream_credit.unwrap_or(0.0));
        let official = official_price_usd(
            &self.model,
            billing.uncached_input_tokens,
            billing.cache_read_input_tokens,
            billing.cache_creation_5m_input_tokens,
            billing.cache_creation_1h_input_tokens,
            self.output_tokens,
        );
        let margin = ((official - actual) * 1_000_000.0).round() / 1_000_000.0;
        // 进程维度累计实际成本/官方价/毛利，供 admin 只读接口查询（无锁原子，零热路径开销）。
        let stop_reason = self.state_manager.get_stop_reason();
        let truncated = stop_reason == "max_tokens";
        super::billing_stats().record(actual, official, margin, truncated);
        // TTFT（首字耗时）：从「向上游发出请求」（ttft_origin，未注入则回退 start）到
        // 上游首字节。以发出请求为原点才能量到上游"等首 token"的等待——流式下上游常等
        // 首 token 生成好才 flush 响应头，若从响应头起算几乎恒为 0。
        // 缺失（None）记 0，通常只发生在空流/未出首字节即报错的退化场景。
        let ttft_base = self.ttft_origin.unwrap_or(self.start);
        let ttft_ms = self
            .first_byte_at
            .map(|t| t.saturating_duration_since(ttft_base).as_millis() as u64)
            .unwrap_or(0);
        tracing::info!(
            model = %self.model,
            input_tokens = billing.uncached_input_tokens.max(1),
            cache_read = billing.cache_read_input_tokens,
            cache_creation = billing.cache_creation_input_tokens,
            output_tokens = self.output_tokens,
            total_tokens = billing.cache_read_input_tokens
                + billing.cache_creation_input_tokens
                + billing.uncached_input_tokens,
            upstream_credit = self.upstream_credit.unwrap_or(0.0),
            actual_cost_usd = actual,
            official_price_usd = official,
            margin_usd = margin,
            stop_reason = %stop_reason,
            ttft_ms = ttft_ms,
            elapsed_secs = self.start.elapsed().as_secs_f64(),
            "请求完成（流式）"
        );

        // 把 cache usage 透传给 state_manager，让 message_delta 输出 cache_* 字段
        self.state_manager.set_final_usage(billing);

        // 生成最终事件（input_tokens 用计费口径的 uncached；对齐 Anthropic
        // 行为保底 1：即使 breakpoint 之后无新内容，官方也不会返回 0）
        events.extend(
            self.state_manager
                .generate_final_events(billing.uncached_input_tokens.max(1), self.output_tokens),
        );
        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner =
            StreamContext::new_with_thinking(model, estimated_input_tokens, thinking_enabled, tool_name_map);
        Self {
            inner,
            event_buffer: Vec::new(),
            initial_events_generated: false,
        }
    }

    /// 注入 prompt caching 结果（透传到内部 StreamContext）
    pub fn set_cache_usage(&mut self, ctx: CacheUsageContext) {
        self.inner.set_cache_usage(ctx);
    }

    /// 注入计费回写句柄（透传到内部 StreamContext）
    pub fn set_billing_writeback(&mut self, tracker: Arc<CacheTracker>, writeback: CacheWriteback) {
        self.inner.set_billing_writeback(tracker, writeback);
    }

    /// 设置 TTFT 计时原点（透传到内部 StreamContext）
    pub fn set_ttft_origin(&mut self, origin: Instant) {
        self.inner.set_ttft_origin(origin);
    }

    /// 标记上游首字节到达时刻（透传到内部 StreamContext，用于 TTFT 统计）
    pub fn mark_first_byte(&mut self) {
        self.inner.mark_first_byte();
    }

    /// 请求的模型名称（仅用于诊断日志）
    pub fn model(&self) -> &str {
        &self.inner.model
    }

    /// 已累计的输出 token 估算（仅用于诊断日志）
    pub fn output_tokens(&self) -> i32 {
        self.inner.output_tokens
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件（message_delta 已在内部用计费值生成）
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // message_start 是流开始时用本地估算口径缓冲的；若收到 contextUsageEvent，
        // 用计费口径（按上游实际总量缩放后的 cache_* 与 uncached）整体回填其 usage，
        // 使 message_start 与 message_delta 一致。
        let billing = self.inner.billing_cache_usage();
        for event in &mut self.event_buffer {
            if event.event != "message_start" {
                continue;
            }
            if let Some(message) = event
                .data
                .get_mut("message")
                .and_then(|message| message.as_object_mut())
            {
                message.insert(
                    "usage".to_string(),
                    serde_json::json!({
                        "input_tokens": billing.uncached_input_tokens.max(1),
                        "output_tokens": 1,
                        "cache_creation_input_tokens": billing.cache_creation_input_tokens,
                        "cache_read_input_tokens": billing.cache_read_input_tokens,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": billing.cache_creation_5m_input_tokens,
                            "ephemeral_1h_input_tokens": billing.cache_creation_1h_input_tokens,
                        }
                    }),
                );
            }
        }

        std::mem::take(&mut self.event_buffer)
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_reasoning_content_event_emits_thinking_then_text() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        // 新端点：先来 reasoningContentEvent（文本增量 + 签名），再来正文。
        let r1: ReasoningContentEvent = serde_json::from_str(r#"{"text":"Let me"}"#).unwrap();
        let r2: ReasoningContentEvent = serde_json::from_str(r#"{"text":" think"}"#).unwrap();
        let sig: ReasoningContentEvent = serde_json::from_str(r#"{"signature":"EuYCabc"}"#).unwrap();

        let mut all = Vec::new();
        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(r1)));
        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(r2)));
        all.extend(ctx.process_kiro_event(&Event::ReasoningContent(sig)));
        let resp: crate::kiro::model::events::AssistantResponseEvent =
            serde_json::from_str(r#"{"content":"答案是 23"}"#).unwrap();
        all.extend(ctx.process_kiro_event(&Event::AssistantResponse(resp)));

        // thinking 块开启
        let start = all
            .iter()
            .find(|e| e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking");
        assert!(start.is_some(), "应有 thinking 块的 content_block_start");

        // thinking_delta 文本拼接
        let thinking: String = all
            .iter()
            .filter(|e| e.data["delta"]["type"] == "thinking_delta")
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(thinking, "Let me think");

        // signature_delta 透传
        let has_sig = all
            .iter()
            .any(|e| e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "EuYCabc");
        assert!(has_sig, "应透传 signature_delta");

        // thinking 块在正文前收尾
        let has_stop = all.iter().any(|e| e.event == "content_block_stop");
        assert!(has_stop, "thinking 块应被 content_block_stop 收尾");

        // 正文以 text_delta 输出
        let text: String = all
            .iter()
            .filter(|e| e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(text, "答案是 23");
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("short_abc12345".to_string(), "mcp__very_long_original_tool_name".to_string());

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events.iter().find(|e| e.event == "content_block_start").unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"],
            "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| !e.data["delta"]["thinking"].as_str().unwrap_or("").is_empty())
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events =
            ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
            })
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
            })
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "abc", "thinking should be 'abc', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(thinking, "hello", "thinking should be 'hello', got: {:?}", thinking);

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        }));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    /// billed_split：无历史 billed 值（None）时回退到缩放本地 cache_read，
    /// 剩余按本地 creation/uncached 比例劈分，三者之和恰为上游总量。
    #[test]
    fn billed_split_falls_back_to_scaled_read_when_no_billed() {
        let estimated = CacheUsage {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
            cache_creation_5m_input_tokens: 300,
            cache_creation_1h_input_tokens: 0,
            uncached_input_tokens: 500,
        };
        // 估算总量 1000，上游实际 2000 → scale = 2，read 缩放为 400。
        let split = estimated.billed_split(1000, 2000, None);
        assert_eq!(split.cache_read_input_tokens, 400, "无 billed 值时缩放本地 read");
        // 剩余 1600 按本地 creation:uncached = 300:500 劈分 → creation=600, uncached=1000。
        assert_eq!(split.cache_creation_input_tokens, 600);
        assert_eq!(split.uncached_input_tokens, 1000);
        // 总量精确。
        assert_eq!(
            split.cache_read_input_tokens
                + split.cache_creation_input_tokens
                + split.uncached_input_tokens,
            2000
        );
        // 5m + 1h == cache_creation 不变量。
        assert_eq!(
            split.cache_creation_5m_input_tokens + split.cache_creation_1h_input_tokens,
            split.cache_creation_input_tokens
        );
    }

    /// billed_split 核心：钉住 cache_read 到历史 billed 值（W），实现读写守恒，
    /// 同时三者之和仍恰为上游实际总量。
    #[test]
    fn billed_split_pins_read_to_billed_and_preserves_total() {
        let estimated = CacheUsage {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 180, // 本地估算的 read（与历史 billed 不同）
            cache_creation_5m_input_tokens: 300,
            cache_creation_1h_input_tokens: 0,
            uncached_input_tokens: 500,
        };
        // 上一次该前缀被计费为 W=450（上游口径），本次上游总量 T=2000。
        let split = estimated.billed_split(1000, 2000, Some(450));
        assert_eq!(
            split.cache_read_input_tokens, 450,
            "cache_read 必须原样钉到历史 billed 值（读写守恒）"
        );
        // 剩余 T-W = 1550 按 creation:uncached = 300:500 劈分。
        assert_eq!(split.cache_creation_input_tokens + split.uncached_input_tokens, 1550);
        // 总量精确：r + c + u == T。
        assert_eq!(
            split.cache_read_input_tokens
                + split.cache_creation_input_tokens
                + split.uncached_input_tokens,
            2000
        );
        assert_eq!(
            split.cache_creation_5m_input_tokens + split.cache_creation_1h_input_tokens,
            split.cache_creation_input_tokens
        );
    }

    /// billed_split 退化边界：W > T 时截断 r=min(W,T)=T，creation/uncached 归零，
    /// 总量仍精确（守恒尽力而为）。
    #[test]
    fn billed_split_caps_read_when_billed_exceeds_total() {
        let estimated = CacheUsage {
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 100,
            cache_creation_5m_input_tokens: 100,
            uncached_input_tokens: 50,
            ..Default::default()
        };
        // 历史 billed W=3000 比本次上游总量 T=2000 还大。
        let split = estimated.billed_split(1000, 2000, Some(3000));
        assert_eq!(split.cache_read_input_tokens, 2000, "r 截断到 T");
        assert_eq!(split.cache_creation_input_tokens, 0);
        assert_eq!(split.uncached_input_tokens, 0);
        assert_eq!(
            split.cache_read_input_tokens
                + split.cache_creation_input_tokens
                + split.uncached_input_tokens,
            2000,
            "总量仍精确"
        );
    }

    /// estimated_total/context_total 非法（<=0）时原样返回。
    #[test]
    fn billed_split_noops_on_invalid_totals() {
        let est = CacheUsage {
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 20,
            uncached_input_tokens: 30,
            ..Default::default()
        };
        assert_eq!(est.billed_split(0, 2000, Some(5)).uncached_input_tokens, 30);
        assert_eq!(est.billed_split(1000, 0, Some(5)).cache_read_input_tokens, 20);
    }

    /// 无 contextUsageEvent 时，计费口径回退到 cache_tracker 的本地估算拆分。
    #[test]
    fn billing_falls_back_to_estimated_split_without_context_event() {
        use super::super::handlers::CacheUsageContext;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1000, false, HashMap::new());
        ctx.set_cache_usage(CacheUsageContext {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
            cache_creation_5m_input_tokens: 300,
            uncached_input_tokens: 500,
            ..Default::default()
        });

        let billing = ctx.billing_cache_usage();
        assert_eq!(billing.uncached_input_tokens, 500);
        assert_eq!(billing.cache_read_input_tokens, 200);
        assert_eq!(billing.cache_creation_input_tokens, 300);

        let _ = ctx.generate_initial_events();
        let events = ctx.generate_final_events();
        let message_delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta");
        assert_eq!(message_delta.data["usage"]["input_tokens"], 500);
        assert_eq!(message_delta.data["usage"]["cache_read_input_tokens"], 200);
    }

    /// 有 contextUsageEvent 时，message_delta 的 usage 用上游实际计量（等比缩放）。
    #[test]
    fn billing_scales_split_to_upstream_when_event_present() {
        use super::super::handlers::CacheUsageContext;

        // 估算总量 1000；上游实际 2000 → scale = 2。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1000, false, HashMap::new());
        ctx.set_cache_usage(CacheUsageContext {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
            cache_creation_5m_input_tokens: 300,
            uncached_input_tokens: 500,
            ..Default::default()
        });
        ctx.context_input_tokens = Some(2000);

        let billing = ctx.billing_cache_usage();
        assert_eq!(billing.uncached_input_tokens, 1000);
        assert_eq!(billing.cache_read_input_tokens, 400);
        assert_eq!(billing.cache_creation_input_tokens, 600);

        let _ = ctx.generate_initial_events();
        let events = ctx.generate_final_events();
        let message_delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta");
        assert_eq!(message_delta.data["usage"]["input_tokens"], 1000);
        assert_eq!(message_delta.data["usage"]["cache_read_input_tokens"], 400);
        assert_eq!(
            message_delta.data["usage"]["cache_creation_input_tokens"],
            600
        );
    }

    /// 全部内容都命中缓存（估算 uncached=0）时，缩放后 uncached=0，message_delta 保底 1。
    #[test]
    fn billing_floors_reported_uncached_at_one_when_fully_cached() {
        use super::super::handlers::CacheUsageContext;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1000, false, HashMap::new());
        ctx.set_cache_usage(CacheUsageContext {
            cache_creation_input_tokens: 500,
            cache_read_input_tokens: 500,
            cache_creation_5m_input_tokens: 500,
            uncached_input_tokens: 0,
            ..Default::default()
        });
        ctx.context_input_tokens = Some(2000);

        // 缩放后 uncached 仍为 0（全部是缓存）。
        assert_eq!(ctx.billing_cache_usage().uncached_input_tokens, 0);

        let _ = ctx.generate_initial_events();
        let events = ctx.generate_final_events();
        let message_delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta");
        // 对外报告保底 1（对齐 Anthropic：不返回 0）。
        assert_eq!(message_delta.data["usage"]["input_tokens"], 1);
    }

    /// 缓冲流：收到 contextUsageEvent 后，message_start 与 message_delta 的
    /// 整个 usage（input_tokens + cache_*）都用上游计量且彼此一致。
    #[test]
    fn buffered_stream_rewrites_message_start_with_scaled_usage() {
        use super::super::handlers::CacheUsageContext;

        let estimated_total = 1000;
        let mut ctx =
            BufferedStreamContext::new("test-model", estimated_total, false, HashMap::new());
        ctx.set_cache_usage(CacheUsageContext {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
            cache_creation_5m_input_tokens: 300,
            uncached_input_tokens: 500,
            ..Default::default()
        });

        // 首个事件即触发 message_start 缓冲；喂入 contextUsageEvent 提供上游总量。
        ctx.process_and_buffer(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 50.0,
            },
        ));

        let window = super::super::converter::get_context_window_size("test-model");
        let context_total = (50.0 * window as f64 / 100.0) as i32;
        // 用相同公式算期望值，避免硬编码窗口大小。
        let expected = CacheUsage {
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 200,
            cache_creation_5m_input_tokens: 300,
            cache_creation_1h_input_tokens: 0,
            uncached_input_tokens: 500,
        }
        .billed_split(estimated_total, context_total, None);

        let events = ctx.finish_and_get_all_events();
        let start = events
            .iter()
            .find(|e| e.event == "message_start")
            .expect("should have message_start");
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta");

        let start_usage = &start.data["message"]["usage"];
        assert_eq!(
            start_usage["input_tokens"],
            expected.uncached_input_tokens.max(1)
        );
        assert_eq!(
            start_usage["cache_read_input_tokens"],
            expected.cache_read_input_tokens
        );
        assert_eq!(
            start_usage["cache_creation_input_tokens"],
            expected.cache_creation_input_tokens
        );
        // message_start 与 message_delta 的 input_tokens 一致。
        assert_eq!(start_usage["input_tokens"], delta.data["usage"]["input_tokens"]);
        assert_eq!(
            delta.data["usage"]["cache_read_input_tokens"],
            expected.cache_read_input_tokens
        );
    }
}

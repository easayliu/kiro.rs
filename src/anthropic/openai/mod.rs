//! OpenAI 原生 API 兼容层（仅 GPT 模型）
//!
//! 在既有 Anthropic 管线之上加一层格式适配，暴露 OpenAI 原生端点：
//! - `POST /v1/chat/completions` — Chat Completions（流式 + 非流式）
//! - `POST /v1/responses` — Responses API（非流式；流式后续）
//!
//! 请求转成 Anthropic 形状 → 复用 `handle_messages` → 响应转码回 OpenAI 形状。
//! 凭证选择、缓存记账、计费统计、截断/metadata 处理全部原样复用，不重复实现。

mod convert;
mod handlers;
mod transcode;

pub use handlers::{post_chat_completions, post_responses};

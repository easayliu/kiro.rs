//! 推理（思考）内容事件
//!
//! 处理 `reasoningContentEvent` 类型的事件。新版 Kiro runtime 端点
//! (`runtime.{region}.kiro.dev`) 在开启 thinking（`additionalModelRequestFields.thinking`
//! = adaptive）时，会用独立的 `reasoningContentEvent` 逐 token 流式下发思考内容，
//! payload 形如 `{"text":" The"}`，并在末尾用 `{"signature":"..."}` 携带思考签名。
//!
//! 这与老 CodeWhisperer 端点把思考内联进 `assistantResponseEvent` 文本的
//! `<thinking>...</thinking>` 标签方式不同，需单独成块解析。

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 推理内容事件
///
/// 每帧二选一：`text`（思考文本增量）或 `signature`（思考签名，通常在最后一帧）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// 思考文本增量
    #[serde(default)]
    pub text: String,

    /// 思考签名（Bedrock/CodeWhisperer 侧签名，通常在思考流末尾单独一帧下发）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    /// 捕获其他未使用的字段，确保反序列化兼容性
    #[serde(flatten)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    extra: serde_json::Value,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_text() {
        let event: ReasoningContentEvent = serde_json::from_str(r#"{"text":" The"}"#).unwrap();
        assert_eq!(event.text, " The");
        assert!(event.signature.is_none());
    }

    #[test]
    fn test_deserialize_signature() {
        let event: ReasoningContentEvent =
            serde_json::from_str(r#"{"signature":"EuYCabc"}"#).unwrap();
        assert_eq!(event.text, "");
        assert_eq!(event.signature.as_deref(), Some("EuYCabc"));
    }
}

//! 元数据事件
//!
//! 处理 metadataEvent 类型的事件
//!
//! 这是上游 generateAssistantResponse 流在生成结束时下发的**权威收尾信号**，
//! 形如 `{"stopReason":"END_TURN"}`，出现在末尾 contextUsageEvent / meteringEvent
//! 之前，是整条流里第一个明确"模型已收笔"的标记。相较依赖 meteringEvent（计费帧、
//! 偶发漏发）反推，它更早也更可靠。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 元数据事件
///
/// 承载上游的结束原因（`stopReason`），供映射为 Anthropic 的 `stop_reason`。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 上游结束原因，如 `END_TURN` / `MAX_TOKENS` / `TOOL_USE` / `STOP_SEQUENCE`
    #[serde(default)]
    pub stop_reason: Option<String>,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl MetadataEvent {
    /// 把上游 `stopReason` 映射为 Anthropic Messages API 的 `stop_reason`。
    ///
    /// 仅透传已知且被客户端 schema 接受的值；未知值返回 `None`（回落到本地推断，
    /// 避免把 `CONTENT_FILTERED` 等非法枚举直接塞给客户端而被拒）。未知值会记
    /// 一条 warn，便于日后按上游新增值补全映射。
    pub fn anthropic_stop_reason(&self) -> Option<String> {
        let raw = self.stop_reason.as_deref()?;
        let mapped = match raw {
            "END_TURN" => "end_turn",
            "MAX_TOKENS" => "max_tokens",
            "TOOL_USE" => "tool_use",
            "STOP_SEQUENCE" => "stop_sequence",
            other => {
                tracing::warn!(
                    "metadataEvent 携带未知 stopReason={:?}，回落本地推断（未映射）",
                    other
                );
                return None;
            }
        };
        Some(mapped.to_string())
    }
}

impl std::fmt::Display for MetadataEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopReason={:?}", self.stop_reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(reason: Option<&str>) -> MetadataEvent {
        MetadataEvent {
            stop_reason: reason.map(|s| s.to_string()),
        }
    }

    #[test]
    fn parses_real_metadata_payload() {
        // 抓包实测 kiro-response.raw 尾部的 metadataEvent 负载
        let e: MetadataEvent = serde_json::from_str(r#"{"stopReason":"END_TURN"}"#).unwrap();
        assert_eq!(e.stop_reason.as_deref(), Some("END_TURN"));
        assert_eq!(e.anthropic_stop_reason().as_deref(), Some("end_turn"));
    }

    #[test]
    fn maps_known_stop_reasons() {
        assert_eq!(ev(Some("END_TURN")).anthropic_stop_reason().as_deref(), Some("end_turn"));
        assert_eq!(ev(Some("MAX_TOKENS")).anthropic_stop_reason().as_deref(), Some("max_tokens"));
        assert_eq!(ev(Some("TOOL_USE")).anthropic_stop_reason().as_deref(), Some("tool_use"));
        assert_eq!(ev(Some("STOP_SEQUENCE")).anthropic_stop_reason().as_deref(), Some("stop_sequence"));
    }

    #[test]
    fn unknown_or_missing_falls_back_to_none() {
        // 未知值不透传给客户端（避免非法枚举被拒），回落本地推断
        assert_eq!(ev(Some("CONTENT_FILTERED")).anthropic_stop_reason(), None);
        assert_eq!(ev(None).anthropic_stop_reason(), None);
    }

    #[test]
    fn missing_field_defaults_to_none() {
        let e: MetadataEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(e.stop_reason, None);
    }
}

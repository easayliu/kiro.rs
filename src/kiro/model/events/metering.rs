//! 计费事件
//!
//! 处理 meteringEvent 类型的事件。
//!
//! 上游在响应流末尾返回本次请求的真实扣费，单位是 **credit**（不是 token）：
//! `{"unit":"credit","unitPlural":"credits","usage":0.0966...}`。
//! 这是上游对该请求的实际计费 ground truth，可用于成本对账 / 毛利监控。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
///
/// `usage` 是上游真实扣费金额，单位由 `unit` 给出（实测为 `credit`）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位（实测 "credit"）
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数形式（实测 "credits"）
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求的真实扣费金额（上游计量 ground truth）
    #[serde(default)]
    pub usage: f64,
}

impl EventPayload for MeteringEvent {
    /// 宽松解析：payload 缺失或格式异常时退回默认值（usage=0），
    /// 避免单个 metering 帧解析失败中断整个响应流。
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        Ok(frame.payload_as_json().unwrap_or_default())
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unit = if self.usage == 1.0 {
            &self.unit
        } else {
            &self.unit_plural
        };
        write!(f, "{} {}", self.usage, unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析上游真实 meteringEvent payload（来自实际抓包）。
    #[test]
    fn parses_real_metering_payload() {
        let raw = r#"{"unit":"credit","unitPlural":"credits","usage":0.09668406437810946}"#;
        let m: MeteringEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(m.unit, "credit");
        assert_eq!(m.unit_plural, "credits");
        assert!((m.usage - 0.09668406437810946).abs() < 1e-12);
    }

    /// 字段缺失时退回默认（usage=0），不报错。
    #[test]
    fn missing_fields_default_to_zero() {
        let m: MeteringEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(m.usage, 0.0);
        assert!(m.unit.is_empty());
    }
}

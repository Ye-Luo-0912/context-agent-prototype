use crate::OpenAiProtocol;
use serde_json::{Value, json};

/// An explicit Responses setting, not inferred from a model alias.
/// Endpoints may support only a subset; rejection remains visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResponsesReasoningEffort {
    #[default]
    ProviderDefault,
    None,
    Low,
    High,
    Max,
}

impl ResponsesReasoningEffort {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "provider_default" => Ok(Self::ProviderDefault),
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err("OPENAI_RESPONSES_REASONING_EFFORT must be provider_default, none, low, high, or max".into()),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderDefault => "provider_default",
            Self::None => "none",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub fn validate_protocol(self, protocol: OpenAiProtocol) -> Result<(), String> {
        if self != Self::ProviderDefault && protocol != OpenAiProtocol::Responses {
            return Err(
                "OPENAI_RESPONSES_REASONING_EFFORT requires OPENAI_API_PROTOCOL=responses".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn apply(self, payload: &mut Value) {
        if self != Self::ProviderDefault {
            payload["reasoning"] = json!({"effort":self.as_str()});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_effort_is_strict_and_default_leaves_payload_unchanged() {
        let original = json!({"model":"fixture","input":[{"role":"system","content":"keep me"}]});
        let mut wire = original.clone();
        ResponsesReasoningEffort::ProviderDefault.apply(&mut wire);
        assert_eq!(wire, original);
        assert!(ResponsesReasoningEffort::parse("bogus").is_err());
        for value in ["none", "low", "high", "max"] {
            let effort = ResponsesReasoningEffort::parse(value).unwrap();
            assert!(effort.validate_protocol(OpenAiProtocol::Responses).is_ok());
            assert!(effort.validate_protocol(OpenAiProtocol::Auto).is_err());
            assert!(
                effort
                    .validate_protocol(OpenAiProtocol::ChatCompletions)
                    .is_err()
            );
            effort.apply(&mut wire);
            assert_eq!(wire["reasoning"], json!({"effort":value}));
            assert_eq!(wire["input"], original["input"]);
        }
    }
}

use crate::OpenAiProtocol;
use serde_json::{Value, json};

/// Explicit opt-in to the Chat `thinking` extension. Enabling thinking is
/// intentionally unsupported until reasoning history has a complete transport
/// contract; the default continues to leave the provider's behavior unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChatThinkingMode {
    #[default]
    ProviderDefault,
    Disabled,
}

impl ChatThinkingMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "provider_default" => Ok(Self::ProviderDefault),
            "disabled" => Ok(Self::Disabled),
            _ => Err("OPENAI_CHAT_THINKING must be provider_default or disabled; enabled thinking history is not supported".into()),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderDefault => "provider_default",
            Self::Disabled => "disabled",
        }
    }

    pub fn validate_protocol(self, protocol: OpenAiProtocol) -> Result<(), String> {
        if self != Self::ProviderDefault && protocol != OpenAiProtocol::ChatCompletions {
            return Err("OPENAI_CHAT_THINKING requires OPENAI_API_PROTOCOL=chat".into());
        }
        Ok(())
    }

    pub(crate) fn apply(self, payload: &mut Value) {
        if self == Self::Disabled {
            payload["thinking"] = json!({"type":"disabled"});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preserves_wire_and_explicit_disable_requires_chat() {
        let original = json!({"model":"fixture","messages":[{"role":"user","content":"read"}]});
        let mut wire = original.clone();
        ChatThinkingMode::ProviderDefault.apply(&mut wire);
        assert_eq!(wire, original);
        for protocol in [
            OpenAiProtocol::Auto,
            OpenAiProtocol::Responses,
            OpenAiProtocol::ChatCompletions,
        ] {
            assert!(
                ChatThinkingMode::ProviderDefault
                    .validate_protocol(protocol)
                    .is_ok()
            );
        }
        let disabled = ChatThinkingMode::parse("disabled").unwrap();
        assert!(disabled.validate_protocol(OpenAiProtocol::Auto).is_err());
        assert!(
            disabled
                .validate_protocol(OpenAiProtocol::Responses)
                .is_err()
        );
        assert!(
            disabled
                .validate_protocol(OpenAiProtocol::ChatCompletions)
                .is_ok()
        );
        disabled.apply(&mut wire);
        assert_eq!(wire["thinking"], json!({"type":"disabled"}));
        wire.as_object_mut().unwrap().remove("thinking");
        assert_eq!(wire, original);
        for value in ["enabled", "none", "low", "garbage"] {
            assert!(ChatThinkingMode::parse(value).is_err());
        }
    }
}

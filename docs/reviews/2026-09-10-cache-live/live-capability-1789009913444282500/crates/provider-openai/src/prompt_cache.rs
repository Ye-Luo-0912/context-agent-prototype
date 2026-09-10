use crate::OpenAiProtocol;

/// Explicit endpoint capability/configuration, never inferred from its URL,
/// model alias or OpenAI-compatible protocol. Default sends no cache fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenAiPromptCacheMode {
    #[default]
    ProviderDefault,
    /// Map a valid common boundary to Responses content-block caching.
    /// Opt in only for an endpoint/model known to support this extension.
    ResponsesExplicit,
}

impl OpenAiPromptCacheMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "provider_default" => Ok(Self::ProviderDefault),
            "responses_explicit" => Ok(Self::ResponsesExplicit),
            _ => Err(
                "OPENAI_PROMPT_CACHE_MODE must be provider_default or responses_explicit".into(),
            ),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderDefault => "provider_default",
            Self::ResponsesExplicit => "responses_explicit",
        }
    }

    pub fn validate_protocol(self, protocol: OpenAiProtocol) -> Result<(), String> {
        if self == Self::ResponsesExplicit && protocol != OpenAiProtocol::Responses {
            return Err(
                "OPENAI_PROMPT_CACHE_MODE=responses_explicit requires OPENAI_API_PROTOCOL=responses; auto/chat cannot guarantee this capability".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

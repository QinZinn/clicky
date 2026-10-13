//! The catalogue of LLM providers Clicky knows how to talk to.
//!
//! Almost every provider speaks one of three wire formats, so a "provider" is
//! mostly a base URL plus a few defaults. Anything not listed here can use the
//! `custom` entry with any OpenAI-compatible endpoint.

use crate::config::ApiStyle;

/// The request/response protocol used to talk to a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFormat {
    /// `POST {base}/chat/completions` (OpenAI, OpenRouter, DeepSeek, Ollama, ...)
    OpenAiChat,
    /// `POST {base}/responses` (OpenAI's newer API)
    OpenAiResponses,
    /// `POST {base}/messages` (Anthropic)
    AnthropicMessages,
}

impl WireFormat {
    pub fn path(self) -> &'static str {
        match self {
            WireFormat::OpenAiChat => "chat/completions",
            WireFormat::OpenAiResponses => "responses",
            WireFormat::AnthropicMessages => "messages",
        }
    }
}

/// How a preset decides which wire format a given model needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatRule {
    Fixed(WireFormat),
    /// OpenCode Zen serves different model families on different endpoints.
    OpenCodeZen,
}

#[derive(Debug)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub format_rule: FormatRule,
    pub default_model: &'static str,
    /// Shown as quick picks before the user fetches the live model list.
    pub suggested_models: &'static [&'static str],
    /// Environment variables checked when no key is stored in settings.
    pub key_env_vars: &'static [&'static str],
    /// False for local servers such as Ollama.
    pub needs_api_key: bool,
    pub vision_by_default: bool,
    pub notes: &'static str,
}

pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com/v1",
        format_rule: FormatRule::Fixed(WireFormat::AnthropicMessages),
        default_model: "claude-sonnet-4-6",
        suggested_models: &[
            "claude-sonnet-4-6",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-haiku-5-5",
        ],
        key_env_vars: &["ANTHROPIC_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "Get a key at console.anthropic.com.",
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "gpt-5.2",
        suggested_models: &["gpt-5.2", "gpt-5.4-mini", "gpt-5.5"],
        key_env_vars: &["OPENAI_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "Get a key at platform.openai.com.",
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "anthropic/claude-sonnet-4.6",
        suggested_models: &[
            "anthropic/claude-sonnet-4.6",
            "google/gemini-2.5-flash",
            "openai/gpt-5-mini",
            "meta-llama/llama-4-maverick",
        ],
        key_env_vars: &["OPENROUTER_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "One key for hundreds of models. 'Refresh models' marks which accept images.",
    },
    ProviderPreset {
        id: "opencode-zen",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        format_rule: FormatRule::OpenCodeZen,
        default_model: "claude-sonnet-4-6",
        suggested_models: &[
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "gpt-5.4-mini",
            "kimi-k2.6",
            "glm-5.2",
            "deepseek-v4-flash",
        ],
        key_env_vars: &["OPENCODE_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        // Verified against the live API: the free-tier models (big-pickle and
        // anything ending in -free) answer other clients with HTTP 403.
        notes: "Needs a paid Zen key. Zen's free-tier models only work inside the OpenCode \
                app itself. Turn off 'Send screenshots' for text-only models.",
    },
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "deepseek-flash",
        suggested_models: &["deepseek-flash", "deepseek-v4-pro"],
        key_env_vars: &["DEEPSEEK_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "Get a key at platform.deepseek.com.",
    },
    ProviderPreset {
        id: "gemini",
        name: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "gemini-flash-latest",
        suggested_models: &["gemini-flash-latest", "gemini-pro-latest"],
        key_env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "Uses Google's OpenAI-compatible endpoint. Key from aistudio.google.com.",
    },
    ProviderPreset {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "meta-llama/llama-4-scout-17b-16e-instruct",
        suggested_models: &["meta-llama/llama-4-scout-17b-16e-instruct"],
        key_env_vars: &["GROQ_API_KEY"],
        needs_api_key: true,
        vision_by_default: true,
        notes: "Very fast. Use 'Refresh models' to see which models are currently offered.",
    },
    ProviderPreset {
        id: "ollama",
        name: "Ollama (local)",
        base_url: "http://localhost:11434/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "llama3.2-vision",
        suggested_models: &["llama3.2-vision", "qwen2.5vl", "gemma3"],
        key_env_vars: &[],
        needs_api_key: false,
        vision_by_default: true,
        notes: "Runs on your machine; nothing leaves it. Pull a vision model first.",
    },
    ProviderPreset {
        id: "lmstudio",
        name: "LM Studio (local)",
        base_url: "http://localhost:1234/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "local-model",
        suggested_models: &[],
        key_env_vars: &[],
        needs_api_key: false,
        vision_by_default: true,
        notes: "Start the LM Studio server, then use 'Refresh models'.",
    },
    ProviderPreset {
        id: "custom",
        name: "Custom (OpenAI-compatible)",
        base_url: "http://localhost:8000/v1",
        format_rule: FormatRule::Fixed(WireFormat::OpenAiChat),
        default_model: "",
        suggested_models: &[],
        key_env_vars: &["CLICKY_CUSTOM_API_KEY"],
        needs_api_key: false,
        vision_by_default: true,
        notes: "Any server that implements /chat/completions: xAI, Mistral, Together, vLLM, ...",
    },
];

pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Why a model cannot be used with the selected provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UnsupportedModel(pub String);

/// Models that OpenCode Zen serves through its Anthropic-style endpoint even
/// though they are not Claude models.
const ZEN_ANTHROPIC_STYLE_NON_CLAUDE_MODELS: &[&str] = &[
    "qwen3.8-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-plus",
    "qwen3.5-plus",
];

/// Picks the endpoint family OpenCode Zen uses for `model`. This mirrors the
/// table in Zen's documentation; override it with the API style setting if
/// Zen moves a model.
pub fn zen_wire_format(model: &str) -> Result<WireFormat, UnsupportedModel> {
    let model = model.trim();
    if model.starts_with("claude-") || ZEN_ANTHROPIC_STYLE_NON_CLAUDE_MODELS.contains(&model) {
        Ok(WireFormat::AnthropicMessages)
    } else if model.starts_with("gpt-") || model.starts_with("grok-") || model.starts_with("muse-")
    {
        Ok(WireFormat::OpenAiResponses)
    } else if model.starts_with("gemini-") {
        Err(UnsupportedModel(
            "OpenCode Zen serves Gemini models through Google's native API, which Clicky does \
             not speak. Use the Google Gemini or OpenRouter provider for Gemini."
                .to_string(),
        ))
    } else if model.starts_with("jev-") {
        Err(UnsupportedModel(
            "This OpenCode Zen model uses a custom endpoint Clicky does not support.".to_string(),
        ))
    } else {
        Ok(WireFormat::OpenAiChat)
    }
}

/// Resolves the wire format for a provider/model pair, honouring a manual
/// override from the settings.
pub fn resolve_wire_format(
    provider_id: &str,
    api_style: ApiStyle,
    model: &str,
) -> Result<WireFormat, UnsupportedModel> {
    match api_style {
        ApiStyle::OpenAiChat => return Ok(WireFormat::OpenAiChat),
        ApiStyle::OpenAiResponses => return Ok(WireFormat::OpenAiResponses),
        ApiStyle::AnthropicMessages => return Ok(WireFormat::AnthropicMessages),
        ApiStyle::Auto => {}
    }
    match preset(provider_id).map(|preset| preset.format_rule) {
        Some(FormatRule::Fixed(format)) => Ok(format),
        Some(FormatRule::OpenCodeZen) => zen_wire_format(model),
        None => Ok(WireFormat::OpenAiChat),
    }
}

/// Removes trailing slashes and any endpoint path the user pasted by mistake
/// (`.../chat/completions`), so `endpoint_url` can append the right one.
pub fn normalize_base_url(base_url: &str) -> String {
    let mut normalized = base_url.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/responses", "/messages", "/models"] {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.to_string();
            break;
        }
    }
    normalized
}

pub fn endpoint_url(base_url: &str, format: WireFormat) -> String {
    format!("{}/{}", normalize_base_url(base_url), format.path())
}

pub fn models_url(base_url: &str) -> String {
    format!("{}/models", normalize_base_url(base_url))
}

/// True when an API key sent to `base_url` would travel unencrypted over the
/// internet: plain `http://` to a host that is not this machine or on a
/// private network. (Local servers such as Ollama legitimately use http.)
pub fn sends_key_over_plain_http_to_remote_host(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(address)) => {
            !(address.is_loopback() || address.is_private() || address.is_link_local())
        }
        Some(url::Host::Ipv6(address)) => !address.is_loopback(),
        Some(url::Host::Domain(domain)) => {
            let domain = domain.to_ascii_lowercase();
            !(domain == "localhost" || domain.ends_with(".local") || domain.ends_with(".localhost"))
        }
        None => false,
    }
}

/// OpenAI's newer models reject `max_tokens` in favour of
/// `max_completion_tokens`, while most compatible servers only know the old
/// name.
pub fn max_tokens_field_name(base_url: &str) -> &'static str {
    if base_url.contains("api.openai.com") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_unique() {
        let mut ids: Vec<_> = PRESETS.iter().map(|preset| preset.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn zen_routes_models_to_the_documented_endpoint() {
        assert_eq!(
            zen_wire_format("claude-sonnet-4-6"),
            Ok(WireFormat::AnthropicMessages)
        );
        assert_eq!(
            zen_wire_format("qwen3.7-plus"),
            Ok(WireFormat::AnthropicMessages)
        );
        assert_eq!(
            zen_wire_format("gpt-5.4-mini"),
            Ok(WireFormat::OpenAiResponses)
        );
        assert_eq!(zen_wire_format("grok-4.6"), Ok(WireFormat::OpenAiResponses));
        assert_eq!(zen_wire_format("kimi-k2.6"), Ok(WireFormat::OpenAiChat));
        assert_eq!(zen_wire_format("big-pickle"), Ok(WireFormat::OpenAiChat));
        assert_eq!(zen_wire_format("qwen3.8-max"), Ok(WireFormat::OpenAiChat));
        assert!(zen_wire_format("gemini-3.5-flash").is_err());
    }

    #[test]
    fn manual_style_overrides_auto_detection() {
        assert_eq!(
            resolve_wire_format("opencode-zen", ApiStyle::OpenAiChat, "claude-sonnet-4-6"),
            Ok(WireFormat::OpenAiChat)
        );
        assert_eq!(
            resolve_wire_format("opencode-zen", ApiStyle::Auto, "claude-sonnet-4-6"),
            Ok(WireFormat::AnthropicMessages)
        );
        assert_eq!(
            resolve_wire_format("deepseek", ApiStyle::Auto, "deepseek-flash"),
            Ok(WireFormat::OpenAiChat)
        );
    }

    #[test]
    fn pasted_endpoint_paths_are_stripped_from_the_base_url() {
        assert_eq!(
            endpoint_url(
                "https://api.deepseek.com/chat/completions",
                WireFormat::OpenAiChat
            ),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            endpoint_url(" https://openrouter.ai/api/v1/ ", WireFormat::OpenAiChat),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            models_url("https://opencode.ai/zen/v1/"),
            "https://opencode.ai/zen/v1/models"
        );
    }

    #[test]
    fn warns_only_about_plain_http_to_remote_hosts() {
        for risky in [
            "http://api.example.com/v1",
            "http://8.8.8.8/v1",
            "http://my-server.net:8000/v1",
        ] {
            assert!(sends_key_over_plain_http_to_remote_host(risky), "{risky}");
        }
        for fine in [
            "https://api.example.com/v1",
            "http://localhost:11434/v1",
            "http://127.0.0.1:8787/v1",
            "http://[::1]:8000/v1",
            "http://192.168.1.20:1234/v1",
            "http://10.0.0.5/v1",
            "http://my-box.local:8000/v1",
            "not a url",
            "",
        ] {
            assert!(!sends_key_over_plain_http_to_remote_host(fine), "{fine}");
        }
    }

    #[test]
    fn only_openai_uses_max_completion_tokens() {
        assert_eq!(
            max_tokens_field_name("https://api.openai.com/v1"),
            "max_completion_tokens"
        );
        assert_eq!(
            max_tokens_field_name("https://openrouter.ai/api/v1"),
            "max_tokens"
        );
    }
}

//! Wire-name ↔ `genai::adapter::AdapterKind` mapping.
//!
//! The `kind` string on `ProviderConfig` is the user-facing identifier
//! for the wire shape (e.g. `"openai-chat"`, `"anthropic"`, `"zai"`). It
//! also rides through to `Provider::kind()` so persisted history rows
//! carry the wire that produced them. This module holds the single
//! source of truth for that mapping.
//!
//! Adding a new wire is a one-line addition to the match arm.

use genai::adapter::AdapterKind;
use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error(
        "unknown provider kind {0:?}; supported: openai-chat, openai-responses, anthropic, gemini, openrouter, zai, deepseek, moonshot, ollama, groq, xai, together, fireworks"
    )]
    UnknownKind(String),
}

/// Parse the `ProviderConfig.kind` string into the genai `AdapterKind`
/// it represents, alongside the canonical `'static` wire-name literal
/// the `Provider` will return from `kind()`.
///
/// The canonical literal is returned (rather than just `kind.as_str()`)
/// so the `&'static str` lifetime requirement of `Provider::kind()` is
/// satisfied without leaking the user's String.
pub fn parse_kind(kind: &str) -> Result<(&'static str, AdapterKind), Error> {
    match kind {
        "openai-chat" => Ok(("openai-chat", AdapterKind::OpenAI)),
        "openai-responses" => Ok(("openai-responses", AdapterKind::OpenAIResp)),
        "anthropic" => Ok(("anthropic", AdapterKind::Anthropic)),
        "gemini" => Ok(("gemini", AdapterKind::Gemini)),
        "openrouter" => Ok(("openrouter", AdapterKind::OpenRouter)),
        "zai" => Ok(("zai", AdapterKind::Zai)),
        "deepseek" => Ok(("deepseek", AdapterKind::DeepSeek)),
        "moonshot" => Ok(("moonshot", AdapterKind::Moonshot)),
        "ollama" => Ok(("ollama", AdapterKind::Ollama)),
        "groq" => Ok(("groq", AdapterKind::Groq)),
        "xai" => Ok(("xai", AdapterKind::Xai)),
        "together" => Ok(("together", AdapterKind::Together)),
        "fireworks" => Ok(("fireworks", AdapterKind::Fireworks)),
        other => Err(Error::UnknownKind(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_each_supported_kind() {
        // Every arm round-trips its own canonical name.
        for name in [
            "openai-chat",
            "openai-responses",
            "anthropic",
            "gemini",
            "openrouter",
            "zai",
            "deepseek",
            "moonshot",
            "ollama",
            "groq",
            "xai",
            "together",
            "fireworks",
        ] {
            let (canon, _adapter) =
                parse_kind(name).unwrap_or_else(|_| panic!("{name} unsupported"));
            assert_eq!(canon, name, "canonical name must match input");
        }
    }

    #[test]
    fn parse_unknown_kind_errors() {
        let err = parse_kind("not-a-real-wire").unwrap_err();
        assert!(matches!(err, Error::UnknownKind(s) if s == "not-a-real-wire"));
    }

    #[test]
    fn parse_kind_maps_zai_to_zai_adapter() {
        let (_, adapter) = parse_kind("zai").unwrap();
        assert!(matches!(adapter, AdapterKind::Zai));
    }

    #[test]
    fn parse_kind_distinguishes_openai_chat_and_responses() {
        let (_, chat) = parse_kind("openai-chat").unwrap();
        let (_, resp) = parse_kind("openai-responses").unwrap();
        assert!(matches!(chat, AdapterKind::OpenAI));
        assert!(matches!(resp, AdapterKind::OpenAIResp));
    }
}

//! Provider presets and runtime configuration.

use std::path::PathBuf;

use anyhow::{bail, Result};

/// Built-in provider presets. A preset fills in the default upstream and tells
/// the proxy whether TLS interception (and therefore the CA) is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    OpenAI,
    Anthropic,
    Google,
    Ollama,
    Vllm,
    LmStudio,
    Custom,
}

impl Preset {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "openai" => Preset::OpenAI,
            "anthropic" => Preset::Anthropic,
            "google" | "gemini" => Preset::Google,
            "ollama" => Preset::Ollama,
            "vllm" => Preset::Vllm,
            "lmstudio" | "lm-studio" => Preset::LmStudio,
            "custom" => Preset::Custom,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::OpenAI => "openai",
            Preset::Anthropic => "anthropic",
            Preset::Google => "google",
            Preset::Ollama => "ollama",
            Preset::Vllm => "vllm",
            Preset::LmStudio => "lmstudio",
            Preset::Custom => "custom",
        }
    }

    /// Default upstream base URL for the preset (None for `custom`, which
    /// requires `--upstream`).
    pub fn default_upstream(self) -> Option<&'static str> {
        Some(match self {
            Preset::OpenAI => "https://api.openai.com",
            Preset::Anthropic => "https://api.anthropic.com",
            Preset::Google => "https://generativelanguage.googleapis.com",
            Preset::Ollama => "http://localhost:11434",
            Preset::Vllm => "http://localhost:8000",
            Preset::LmStudio => "http://localhost:1234",
            Preset::Custom => return None,
        })
    }

    /// Local presets speak plain HTTP, so the CA / TLS-interception step is
    /// skipped automatically.
    pub fn is_local(self) -> bool {
        matches!(self, Preset::Ollama | Preset::Vllm | Preset::LmStudio)
    }
}

/// Whether the proxy serves as a forward proxy (HTTPS_PROXY / MITM) or a
/// reverse proxy in front of one fixed upstream (the agent points its
/// `base_url` at us). One running server supports both at once — this is just
/// the default target for origin-form requests.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl Upstream {
    pub fn parse(url: &str) -> Result<Self> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            anyhow::anyhow!("upstream must include scheme, e.g. https://api.host")
        })?;
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("bad port in upstream"))?,
            ),
            None => {
                let port = if scheme == "https" { 443 } else { 80 };
                (authority.to_string(), port)
            }
        };
        if host.is_empty() {
            bail!("upstream host is empty");
        }
        Ok(Upstream {
            scheme: scheme.to_string(),
            host,
            port,
        })
    }

    #[allow(dead_code)]
    pub fn is_tls(&self) -> bool {
        self.scheme == "https"
    }
}

/// Default location for CA material: `~/.replaykit/ca`.
pub fn default_ca_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".replaykit")
        .join("ca")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upstreams() {
        let u = Upstream::parse("https://api.openai.com").unwrap();
        assert_eq!(u.host, "api.openai.com");
        assert_eq!(u.port, 443);
        assert!(u.is_tls());

        let u = Upstream::parse("http://localhost:11434/v1").unwrap();
        assert_eq!(u.host, "localhost");
        assert_eq!(u.port, 11434);
        assert!(!u.is_tls());
    }

    #[test]
    fn preset_locality() {
        assert!(Preset::Ollama.is_local());
        assert!(!Preset::OpenAI.is_local());
        assert_eq!(Preset::parse("gemini"), Some(Preset::Google));
    }
}

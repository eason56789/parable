//! Fetching the model catalogue from an upstream provider.
//!
//! Different providers expose it differently, but the common shapes are:
//!   - OpenAI-compatible: `GET {base}/v1/models` → `{"data":[{"id":…}]}`
//!   - Anthropic:        `GET {base}/v1/models` → `{"data":[{"id":…}]}`
//!   - Ollama:           `GET {base}/api/tags`   → `{"models":[{"name":…}]}`
//!
//! We try the shapes in turn and return whatever the upstream gives us, so
//! the settings page can offer "import from upstream" for any provider the
//! user configured.

use crate::config::ProviderConfig;
use crate::error::HarnessError;
use serde::Serialize;
use serde_json::Value;

/// One model the upstream advertises.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamModel {
    /// The id to send in requests.
    pub id: String,
    /// Display name, when the upstream provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Context window, when the upstream reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Whether the model accepts images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Whether the upstream marks it as a reasoning model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// Anything else the upstream said (raw entry), for the UI to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// Fetch the model list for one provider.
pub async fn list_upstream(cfg: &ProviderConfig) -> Result<Vec<UpstreamModel>, HarnessError> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let base = cfg.base_url.trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(HarnessError::Config("Base URL is empty".into()));
    }
    if cfg.key.trim().is_empty() {
        return Err(HarnessError::Config("API Key is empty".into()));
    }

    // Try each known catalogue endpoint; the first that answers wins.
    let candidates: Vec<(String, reqwest::header::HeaderMap)> = vec![
        (
            format!("{base}/v1/models"),
            bearer(&cfg.key),
        ),
        (
            format!("{base}/api/tags"),
            bearer(&cfg.key),
        ),
        (
            format!("{base}/models"),
            bearer(&cfg.key),
        ),
    ];

    let mut last_err = String::new();
    for (url, headers) in candidates {
        match client.get(&url).headers(headers).send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    last_err = format!("HTTP {status}");
                    continue;
                }
                match parse_models(&text) {
                    Some(models) if !models.is_empty() => return Ok(models),
                    _ => {
                        last_err = "response had no model list".into();
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        }
    }
    Err(HarnessError::Config(format!(
        "could not list models from upstream: {last_err}"
    )))
}

fn bearer(key: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {key}")
            .parse()
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
    );
    h.insert("content-type", reqwest::header::HeaderValue::from_static("application/json"));
    h
}

/// Parse the shapes providers use for their catalogue.
fn parse_models(text: &str) -> Option<Vec<UpstreamModel>> {
    let v: Value = serde_json::from_str(text).ok()?;

    // OpenAI / Anthropic: {"data":[{"id":"…"}]}
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        let out: Vec<UpstreamModel> = arr.iter().filter_map(entry_to_model).collect();
        if !out.is_empty() {
            return Some(out);
        }
    }
    // Ollama: {"models":[{"name":"…"}]}
    if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
        let out: Vec<UpstreamModel> = arr.iter().filter_map(entry_to_model).collect();
        if !out.is_empty() {
            return Some(out);
        }
    }
    // Bare array: [{"id":"…"}]
    if let Some(arr) = v.as_array() {
        let out: Vec<UpstreamModel> = arr.iter().filter_map(entry_to_model).collect();
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn entry_to_model(m: &Value) -> Option<UpstreamModel> {
    let id = m
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| m.get("name").and_then(|v| v.as_str()))?
        .to_string();
    if id.is_empty() {
        return None;
    }
    let name = m
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != id)
        .map(|s| s.to_string());
    let context = m
        .get("context_length")
        .or_else(|| m.get("context_window"))
        .or_else(|| m.get("max_context_length"))
        .and_then(|v| v.as_u64());
    let vision = m
        .get("vision")
        .or_else(|| m.get("supports_vision"))
        .or_else(|| m.get("capabilities"))
        .and_then(|v| v.as_bool())
        .or_else(|| {
            m.get("capabilities")
                .and_then(|c| c.get("vision"))
                .and_then(|v| v.as_bool())
        });
    let thinking = m
        .get("thinking")
        .or_else(|| m.get("supports_thinking"))
        .or_else(|| m.get("reasoning"))
        .and_then(|v| v.as_bool());
    Some(UpstreamModel {
        id,
        name,
        context,
        vision,
        thinking,
        raw: Some(m.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shape() {
        let v = r#"{"data":[{"id":"gpt-4o","context_length":128000},{"id":"gpt-4o-mini"}]}"#;
        let m = parse_models(v).expect("must parse");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "gpt-4o");
        assert_eq!(m[0].context, Some(128000));
        assert_eq!(m[1].context, None);
    }

    #[test]
    fn parses_ollama_shape() {
        let v = r#"{"models":[{"name":"qwen3-coder:8b"},{"name":"llama3"}]}"#;
        let m = parse_models(v).expect("must parse");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "qwen3-coder:8b");
    }

    #[test]
    fn parses_bare_array() {
        let v = r#"[{"id":"a"},{"id":"b"}]"#;
        let m = parse_models(v).expect("must parse");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn rejects_non_model_payloads() {
        assert!(parse_models(r#"{"error":"nope"}"#).is_none());
        assert!(parse_models("not json").is_none());
    }
}

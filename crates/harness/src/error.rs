use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl HarnessError {
    /// 阶段四·fallback 链：是否属于可降级重试的瞬时故障。
    /// 覆盖连接失败 / 超时 / 5xx / 429；4xx 鉴权与参数错误不重试。
    pub fn is_retryable(&self) -> bool {
        match self {
            HarnessError::Http(e) => e.is_timeout() || e.is_connect(),
            HarnessError::Provider(msg) => {
                // Provider 侧非成功响应形如 `HTTP 429: ...` / `HTTP 503: ...`。
                let rest = msg.strip_prefix("HTTP ").unwrap_or(msg);
                let code: u16 = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                code == 429 || (500..=599).contains(&code)
            }
            _ => false,
        }
    }

    /// A description aimed at the *model*, so it can self-correct, rather than a
    /// human-facing stack trace.
    pub fn for_model(&self) -> String {
        match self {
            HarnessError::PermissionDenied(d) => {
                format!("The user denied this operation: {d}. Do not retry it; explain or pick a different approach.")
            }
            HarnessError::NotFound(p) => format!("Not found: {p}. Check the path and try again."),
            HarnessError::Tool(t) => format!("Tool error: {t}"),
            HarnessError::Provider(p) => format!("Model provider error: {p}"),
            other => format!("Error: {other}"),
        }
    }

    /// fallback 事件的标准化原因标签：`timeout` | `connect` | `429` | `5xx`。
    /// 不可降级的故障（4xx 鉴权/参数、工具错误等）返回 `"error"`。
    pub fn fallback_reason(&self) -> &'static str {        match self {
            HarnessError::Http(e) if e.is_timeout() => "timeout",
            HarnessError::Http(e) if e.is_connect() => "connect",
            HarnessError::Provider(msg) => {
                let rest = msg.strip_prefix("HTTP ").unwrap_or(msg);
                let code: u16 = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                match code {
                    429 => "429",
                    500..=599 => "5xx",
                    _ => "error",
                }
            }
            _ => "error",
        }
    }
}

/// 是否允许在出流前切换备用模型：**尚未出流** + 错误可重试 + 还有下一条候选。
///
/// 一旦已有 token 产出（`streamed = true`），不再降级——避免两段半截内容混拼，
/// 后续错误走正常错误路径。
pub fn may_fallback(streamed: bool, err: &HarnessError, has_next: bool) -> bool {
    !streamed && has_next && err.is_retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classifies_provider_http_status() {
        let retryable = ["HTTP 429: rate limited", "HTTP 503: overloaded", "HTTP 500: boom"];
        for msg in retryable {
            assert!(
                HarnessError::Provider(msg.to_string()).is_retryable(),
                "{msg} should be retryable"
            );
        }
        let terminal = ["HTTP 401: unauthorized", "HTTP 400: bad request", "HTTP 404: missing"];
        for msg in terminal {
            assert!(
                !HarnessError::Provider(msg.to_string()).is_retryable(),
                "{msg} must not be retried"
            );
        }
        // 无状态码的普通 Provider 消息、其它错误类型均不降级。
        assert!(!HarnessError::Provider("model said something".into()).is_retryable());
        assert!(!HarnessError::Other("io-ish".into()).is_retryable());
        assert!(!HarnessError::Tool("bash failed".into()).is_retryable());
    }

    #[test]
    fn fallback_reason_classifies_standard_labels() {
        assert_eq!(
            HarnessError::Provider("HTTP 429: rate limited".into()).fallback_reason(),
            "429"
        );
        assert_eq!(
            HarnessError::Provider("HTTP 503: overloaded".into()).fallback_reason(),
            "5xx"
        );
        assert_eq!(
            HarnessError::Provider("HTTP 500: boom".into()).fallback_reason(),
            "5xx"
        );
        // 4xx 与无状态码 / 其它类型归为 error，不进 fallback 原因标签。
        assert_eq!(
            HarnessError::Provider("HTTP 401: unauthorized".into()).fallback_reason(),
            "error"
        );
        assert_eq!(
            HarnessError::Provider("model said something".into()).fallback_reason(),
            "error"
        );
        assert_eq!(HarnessError::Tool("bash failed".into()).fallback_reason(), "error");
    }

    #[test]
    fn may_fallback_guards_post_stream_downgrade() {
        let retryable = HarnessError::Provider("HTTP 503: overloaded".into());
        let terminal = HarnessError::Provider("HTTP 401: unauthorized".into());
        // 未出流 + 可重试 + 有候选 → 允许降级。
        assert!(may_fallback(false, &retryable, true));
        // 出流后即便可重试、有候选，也不再降级。
        assert!(!may_fallback(true, &retryable, true));
        // 无可选候选 / 终态错误不降级。
        assert!(!may_fallback(false, &retryable, false));
        assert!(!may_fallback(false, &terminal, true));
    }
}

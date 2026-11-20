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
}

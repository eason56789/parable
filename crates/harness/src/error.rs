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

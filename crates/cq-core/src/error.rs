pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("CommitUnknown: revision {revision}: {message}")]
    CommitUnknown { revision: String, message: String },
    #[error("{code}: {message}")]
    Remote { code: String, message: String },
    #[error("{code}: {message}")]
    Domain { code: &'static str, message: String },
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}
impl Error {
    pub fn code(&self) -> &str {
        match self {
            Self::CommitUnknown { .. } => "CommitUnknown",
            Self::Domain { code, .. } => code,
            Self::Remote { code, .. } => code,
            Self::Io(_) => "Io",
            Self::Json(_) => "Json",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::Domain { message, .. } | Self::Remote { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }
    pub fn candidate_revision(&self) -> Option<&str> {
        if let Self::CommitUnknown { revision, .. } = self {
            Some(revision)
        } else {
            None
        }
    }
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self::Domain {
            code,
            message: message.into(),
        }
    }
}

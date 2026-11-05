use serde::Deserialize;

/// The JSON error body Mattermost returns for failed API calls.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub detailed_error: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub status_code: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned {status}: {}", .error.message)]
    Api { status: u16, error: ApiError },

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("not logged in (no session token)")]
    NotAuthenticated,

    #[error("login succeeded but the server sent no session token")]
    MissingToken,

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The HTTP status code, if this was an API error.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Api { status, .. } => Some(*status),
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }

    /// The Mattermost error id (e.g. `api.user.login.invalid_credentials_email_username`).
    pub fn api_id(&self) -> Option<&str> {
        match self {
            Error::Api { error, .. } => Some(&error.id),
            _ => None,
        }
    }

    /// True if the login failed because of a missing or wrong MFA code
    /// (server id `api.user.check_user_mfa.bad_code.app_error`).
    pub fn is_mfa_error(&self) -> bool {
        self.api_id().is_some_and(|id| id.contains("mfa"))
    }

    /// True if the session token is invalid or expired.
    pub fn is_unauthorized(&self) -> bool {
        self.status() == Some(401)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

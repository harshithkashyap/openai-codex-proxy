use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode};
use serde_json::{Value, json};

pub(crate) fn response_json(status: StatusCode, body: Value) -> Response<Body> {
    let mut resp = Response::new(Body::from(body.to_string()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    Upstream(String),
}

impl ProxyError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub(crate) fn auth(error: impl std::fmt::Display) -> Self {
        Self::Auth(error.to_string())
    }

    pub(crate) fn upstream(error: impl std::fmt::Display) -> Self {
        Self::Upstream(error.to_string())
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Auth(_) => StatusCode::UNAUTHORIZED,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub(crate) fn error_type(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "invalid_request_error",
            Self::Auth(_) => "auth_error",
            Self::Upstream(_) => "proxy_error",
        }
    }

    pub(crate) fn into_response(self) -> Response<Body> {
        response_json(
            self.status(),
            json!({ "error": { "message": self.to_string(), "type": self.error_type() } }),
        )
    }
}

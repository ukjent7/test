use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let kind = if self.0 == StatusCode::BAD_REQUEST {
            "invalid_request_error"
        } else {
            "api_error"
        };
        (
            self.0,
            Json(json!({"type": "error", "error": {"type": kind, "message": self.1}})),
        )
            .into_response()
    }
}

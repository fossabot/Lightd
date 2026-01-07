use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

use crate::config::AuthorizationConfig;

#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<AuthorizationConfig>,
}

impl AuthState {
    pub fn new(config: AuthorizationConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

pub async fn auth_middleware(
    State(auth): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Skip auth if disabled
    if !auth.config.enabled {
        return next.run(request).await;
    }

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok());

    match auth_header {
        Some(header) => {
            // Support both "Bearer <token>" and raw token
            let token = if header.starts_with("Bearer ") {
                &header[7..]
            } else {
                header
            };

            if token == auth.config.token {
                next.run(request).await
            } else {
                unauthorized_response("Invalid authorization token")
            }
        }
        None => unauthorized_response("Missing Authorization header"),
    }
}

fn unauthorized_response(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "Unauthorized",
            "message": message
        })),
    )
        .into_response()
}

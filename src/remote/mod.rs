//! Remote helper for communicating with the Panel
//!
//! Handles commiunications such as event posting and state updates.
//! This will help the server identify container states and events.

use reqwest::Client;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::config::RemoteConfig;

#[derive(Clone)]
pub struct Remote {
    cfg: Option<RemoteConfig>,
    client: Client,
}

impl Remote {
    pub fn new(cfg: Option<RemoteConfig>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { cfg, client }
    }

    pub fn is_enabled(&self) -> bool {
        self.cfg.as_ref().map(|c| c.enabled).unwrap_or(false)
    }

    fn auth_header(&self) -> Option<String> {
        self.cfg.as_ref().map(|c| format!("Bearer {}", c.secret))
    }

    fn base_url(&self) -> Option<&str> {
        self.cfg.as_ref().map(|c| c.url.as_str())
    }

    async fn post(&self, path: &str, payload: Value) -> anyhow::Result<()> {
        let base = match self.base_url() {
            Some(b) => b,
            None => return Ok(()),
        };

        let url = format!("{}{}", base.trim_end_matches('/'), path);

        let req = self.client.post(&url).header("Content-Type", "application/json");

        let req = if let Some(auth) = self.auth_header() {
            req.header("Authorization", auth)
        } else {
            req
        };

        debug!(%url, "posting to panel: {}", payload);

        match req.json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    debug!(%url, status = %resp.status(), "post successful");
                    Ok(())
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    error!(%url, %status, %body, "panel rejected request");
                    Ok(())
                }
            }
            Err(e) => {
                error!(%url, error = %e, "failed to post to panel");
                Ok(())
            }
        }
    }

    /// Send a generic event to the panel at /api/lightd/event
    pub async fn send_event(&self, name: &str, payload: Value) {
        if !self.is_enabled() {
            return;
        }

        let body = json!({"name": name, "payload": payload});
        let _ = self.post("/api/lightd/event", body).await;
    }

    /// Send a container state update
    pub async fn send_container_state(&self, container_uuid: &str, state: &str) {
        if !self.is_enabled() {
            return;
        }

        let body = json!({"container_uuid": container_uuid, "state": state});
        let path = format!("/api/lightd/containers/{}/state", container_uuid);
        let _ = self.post(&path, body).await;
    }

    /// Send install/update status
    pub async fn send_install_status(&self, container_uuid: &str, status: &str, message: Option<&str>) {
        if !self.is_enabled() {
            return;
        }

        let body = json!({"container_uuid": container_uuid, "status": status, "message": message});
        let path = format!("/api/lightd/containers/{}/install_status", container_uuid);
        let _ = self.post(&path, body).await;
    }

    /// Post RU deduction (used by resource monitor)
    pub async fn post_ru_deduction(&self, container_uuid: &str, amount: f64, description: &str) {
        if !self.is_enabled() {
            return;
        }

        let body = json!({"container_id": container_uuid, "amount": amount, "description": description});
        let _ = self.post("/api/lightd/deduct", body).await;
    }
}

// Re-export handy type
pub type RemoteClient = Arc<Remote>;

// Helper macro to construct json without bringing serde_json::json into scope of callers
#[macro_export]
macro_rules! json_val {
    ($($tt:tt)+) => { serde_json::json!($($tt)+) };
}
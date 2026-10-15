//! The HTTP client for the Qubo REST gateway.

use std::fmt;
use std::time::Duration;

use reqwest::Method;
pub use reqwest::Url;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{QuboError, error_for_status};
use crate::session::Session;

/// The Qubo REST gateway.
pub const BASE_URL: &str = "https://srvcapp.platform.quboworld.com:443";

/// The service-provider UUID that scopes every path on the gateway.
const DEFAULT_SERVICE_PROVIDER: &str = "d10e4bfb0153496e8e8bb955f7ebe413";

/// The Android package id the gateway expects in `App-Id`.
const APP_ID: &str = "com.hero.iot";

/// Total request timeout, carrying over the reference client's value.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The service provider every path is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceProvider(String);

impl ServiceProvider {
    pub fn new(uuid: impl Into<String>) -> Self {
        Self(uuid.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ServiceProvider {
    fn from(uuid: &str) -> Self {
        Self(uuid.to_owned())
    }
}

impl Default for ServiceProvider {
    fn default() -> Self {
        Self(DEFAULT_SERVICE_PROVIDER.to_owned())
    }
}

impl fmt::Display for ServiceProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A client for the Qubo REST gateway.
#[derive(Debug, Clone)]
pub struct QuboClient {
    http: reqwest::Client,
    base: Url,
    sp: ServiceProvider,
    device_id: String,
}

impl QuboClient {
    /// Build a client carrying a fresh `Source-Device-Id`.
    pub fn new(base: Url, sp: ServiceProvider) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            sp,
            device_id: Uuid::new_v4().to_string(),
        }
    }

    /// The `Source-Device-Id` this client sends. Persist it across
    /// restarts to keep the account's logged-in device list stable.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The service provider this client's paths are scoped to.
    pub fn service_provider(&self) -> &ServiceProvider {
        &self.sp
    }

    /// GET `path`, relative to the gateway, authenticated when a session
    /// is given.
    pub async fn get_json(&self, path: &str, auth: Option<&Session>) -> Result<Value, QuboError> {
        self.send(Method::GET, path, None, auth).await
    }

    /// POST a JSON `body` to `path`, authenticated when a session is
    /// given.
    pub async fn post_json(
        &self,
        path: &str,
        body: &Value,
        auth: Option<&Session>,
    ) -> Result<Value, QuboError> {
        self.send(Method::POST, path, Some(body), auth).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        auth: Option<&Session>,
    ) -> Result<Value, QuboError> {
        let url = self.url_for(path)?;
        let mut request = self
            .http
            .request(method, url)
            .timeout(REQUEST_TIMEOUT)
            .header("App-Id", APP_ID)
            .header("Source-Device-Id", &self.device_id)
            .header("Content-Type", "application/json");
        request = match auth {
            Some(session) => request
                .header("Authorization", format!("Bearer {}", session.access_token))
                .header("Subscriber-Key", &session.access_token)
                .header("User-UUID", &session.uuid)
                .header("Token-Type", "USER")
                .header("Source", "Android"),
            None => request.header("Source", "USER"),
        };
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        tracing::debug!(status = status.as_u16(), path, "qubo response");

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Object(serde_json::Map::new()));
            }
            return Ok(serde_json::from_str(&text)?);
        }
        Err(error_for_status(status, &text))
    }

    fn url_for(&self, path: &str) -> Result<Url, QuboError> {
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        self.base.join(&path).map_err(|err| QuboError::Http {
            status: None,
            message: format!("invalid path {path}: {err}"),
        })
    }
}

//! The HTTP client for the Qubo REST gateway.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Method;
pub use reqwest::Url;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::{AccountProfile, select_profile};
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
    /// The login session the OTP flow carries from request to verify.
    pending_login: Arc<Mutex<Option<PendingLogin>>>,
    /// The profiles a login is waiting on a choice between: a number that
    /// maps to several accounts.
    pending_profiles: Arc<Mutex<Option<PendingProfiles>>>,
    /// Stamps every pending set so a claim or release from an older set
    /// can never touch a newer one.
    pending_generation: Arc<AtomicU64>,
}

/// The gateway's login session and the number it was issued for; verify
/// refuses anything else, so two browsers cannot cross their codes.
#[derive(Debug, Clone)]
pub(crate) struct PendingLogin {
    pub(crate) uuid: String,
    pub(crate) phone: String,
}

/// Who a request speaks as: a stored session's authenticated headers, or
/// the login family's bare `Source` value with no Authorization.
enum Caller<'a> {
    Session(&'a Session),
    Source(&'a str),
}

/// The profiles a login is waiting on a choice between, and whether a
/// pick has already claimed the slot to mint it.
#[derive(Debug, Clone)]
struct PendingProfiles {
    generation: u64,
    profiles: Vec<AccountProfile>,
    claimed: bool,
}

/// The claimed pick, holding its slot while the mint runs. Dropping it
/// releases the slot for a retry - which also covers a request the server
/// abandoned mid-mint - and never touches a newer picker's claim.
#[derive(Debug)]
pub struct PickedProfile {
    client: QuboClient,
    profile: AccountProfile,
    generation: u64,
}

impl PickedProfile {
    /// The account this pick chose.
    pub fn profile(&self) -> &AccountProfile {
        &self.profile
    }
}

impl Drop for PickedProfile {
    fn drop(&mut self) {
        self.client.release_pending_profiles(self.generation);
    }
}

impl QuboClient {
    /// Build a client carrying a fresh `Source-Device-Id`.
    pub fn new(base: Url, sp: ServiceProvider) -> Self {
        Self::with_device_id(base, sp, Uuid::new_v4().to_string())
    }

    /// Build a client with the caller's persisted `Source-Device-Id`, so
    /// restarts do not add device rows to the account.
    pub fn with_device_id(base: Url, sp: ServiceProvider, device_id: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            sp,
            device_id,
            pending_login: Arc::new(Mutex::new(None)),
            pending_profiles: Arc::new(Mutex::new(None)),
            pending_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn set_pending_login(&self, uuid: &str, phone: &str) {
        *self.lock_pending() = Some(PendingLogin {
            uuid: uuid.to_owned(),
            phone: phone.to_owned(),
        });
    }

    pub(crate) fn pending_login(&self) -> Option<PendingLogin> {
        self.lock_pending().clone()
    }

    pub(crate) fn clear_pending_login(&self) {
        *self.lock_pending() = None;
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<PendingLogin>> {
        self.pending_login
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keep the profiles a login is waiting on a choice between. Each set
    /// carries a fresh generation: a claim or release from an older set
    /// must never touch this one.
    pub fn set_pending_profiles(&self, profiles: Vec<AccountProfile>) {
        let generation = self.pending_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.lock_pending_profiles() = Some(PendingProfiles {
            generation,
            profiles,
            claimed: false,
        });
    }

    /// Claim the pick for `uuid`: the first select takes the slot while
    /// its mint runs, so a racing second one is refused. The claim rides
    /// the returned guard, which releases the slot on every exit.
    pub fn claim_pending_profile(&self, uuid: &str) -> Result<PickedProfile, QuboError> {
        let mut pending = self.lock_pending_profiles();
        let state = pending.as_mut().ok_or(QuboError::Auth {
            code: None,
            message: "no login is waiting for a profile choice".to_owned(),
        })?;
        if state.claimed {
            return Err(QuboError::Auth {
                code: None,
                message: "a pick for this login is already in flight; wait or request a new code"
                    .to_owned(),
            });
        }
        let profile = select_profile(&state.profiles, uuid)?;
        state.claimed = true;
        Ok(PickedProfile {
            client: self.clone(),
            profile,
            generation: state.generation,
        })
    }

    /// The choice was made or abandoned; nothing is pending any more.
    pub fn clear_pending_profiles(&self) {
        *self.lock_pending_profiles() = None;
    }

    /// Nothing was persisted: release the claim `generation` took, if the
    /// slot still holds it.
    fn release_pending_profiles(&self, generation: u64) {
        let mut pending = self.lock_pending_profiles();
        if let Some(state) = pending.as_mut()
            && state.generation == generation
        {
            state.claimed = false;
        }
    }

    fn lock_pending_profiles(&self) -> std::sync::MutexGuard<'_, Option<PendingProfiles>> {
        self.pending_profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        self.send(
            Method::GET,
            path,
            &[],
            &[],
            None,
            auth.map_or(Caller::Source("USER"), Caller::Session),
        )
        .await
    }

    /// POST a JSON `body` to `path`, authenticated when a session is
    /// given.
    pub async fn post_json(
        &self,
        path: &str,
        body: &Value,
        auth: Option<&Session>,
    ) -> Result<Value, QuboError> {
        self.send(
            Method::POST,
            path,
            &[],
            &[],
            Some(body),
            auth.map_or(Caller::Source("USER"), Caller::Session),
        )
        .await
    }

    /// GET `path` with query parameters, authenticated when a session is
    /// given.
    pub async fn get_json_query(
        &self,
        path: &str,
        query: &[(&str, &str)],
        auth: Option<&Session>,
    ) -> Result<Value, QuboError> {
        self.send(
            Method::GET,
            path,
            query,
            &[],
            None,
            auth.map_or(Caller::Source("USER"), Caller::Session),
        )
        .await
    }

    /// POST a JSON `body` to `path` with query parameters, authenticated
    /// when a session is given.
    pub async fn post_json_query(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
        auth: Option<&Session>,
    ) -> Result<Value, QuboError> {
        self.send(
            Method::POST,
            path,
            query,
            &[],
            Some(body),
            auth.map_or(Caller::Source("USER"), Caller::Session),
        )
        .await
    }

    /// POST a JSON `body` to `path` with query parameters and `headers`
    /// added verbatim, no Authorization header, and `Source: ANDROID` -
    /// the shape the app's own login-family calls carry.
    pub async fn post_login_json(
        &self,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value, QuboError> {
        self.send(
            Method::POST,
            path,
            query,
            headers,
            Some(body),
            Caller::Source("ANDROID"),
        )
        .await
    }

    /// PUT a JSON `body` and ignore the body it answers with. Device
    /// commands ack every shape alike, so the ack is not worth parsing.
    pub async fn put_ack(
        &self,
        path: &str,
        body: &Value,
        auth: Option<&Session>,
    ) -> Result<(), QuboError> {
        self.send_raw(
            Method::PUT,
            path,
            &[],
            &[],
            Some(body),
            auth.map_or(Caller::Source("USER"), Caller::Session),
        )
        .await
        .map(drop)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Option<&Value>,
        caller: Caller<'_>,
    ) -> Result<Value, QuboError> {
        let text = self
            .send_raw(method, path, query, headers, body, caller)
            .await?;
        if text.trim().is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn send_raw(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Option<&Value>,
        caller: Caller<'_>,
    ) -> Result<String, QuboError> {
        let url = self.url_for(path, query)?;
        let mut request = self
            .http
            .request(method, url)
            .timeout(REQUEST_TIMEOUT)
            .header("App-Id", APP_ID)
            .header("Source-Device-Id", &self.device_id)
            .header("Content-Type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request = match caller {
            Caller::Session(session) => request
                .header("Authorization", format!("Bearer {}", session.access_token))
                .header("Subscriber-Key", &session.access_token)
                .header("User-UUID", &session.uuid)
                .header("Token-Type", "USER")
                .header("Source", "Android"),
            Caller::Source(source) => request.header("Source", source),
        };
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        tracing::debug!(status = status.as_u16(), path, "qubo response");

        if status.is_success() {
            return Ok(text);
        }
        Err(error_for_status(status, &text))
    }

    fn url_for(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, QuboError> {
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        let mut url = self.base.join(&path).map_err(|err| QuboError::Http {
            status: None,
            message: format!("invalid path {path}: {err}"),
        })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }
}

//! Durable Qubo session state.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::QuboError;

/// A logged-in Qubo account session. Both login routes mint a refresh
/// token - the phone route through the app's profile-select mint, Google
/// through the GM login - and it is what makes the session durable
/// (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub access_token: String,
    pub uuid: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// A session file at a caller-provided path.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the stored session; `None` means none has been saved yet.
    pub fn load(&self) -> Result<Option<Session>, QuboError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Replace the stored session, readable only by its owner.
    pub fn save(&self, session: &Session) -> Result<(), QuboError> {
        let raw = serde_json::to_vec_pretty(session)?;
        // Write-then-rename so a crash mid-write cannot leave a torn
        // session behind; re-login needs the phone owner. The temp file is
        // unique per writer (0600), so two savers cannot tear each other's.
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(&raw)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|err| err.error)?;
        Ok(())
    }
}

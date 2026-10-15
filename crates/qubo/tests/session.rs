//! Session store: round-trip, file permissions, and the no-session case.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use qubo::session::{Session, SessionStore};

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("nazar-session-{}.json", uuid::Uuid::new_v4()))
}

#[test]
fn save_then_load_round_trips_with_owner_only_permissions() {
    let path = temp_path();
    let store = SessionStore::new(&path);
    let session = Session {
        access_token: "token-abc".into(),
        uuid: "user-uuid-1".into(),
        email: Some("family@example.com".into()),
    };

    store.save(&session).expect("save succeeds");
    let mode = fs::metadata(&path)
        .expect("saved file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the session file is owner-only");

    let loaded = store.load().expect("load succeeds");
    assert_eq!(loaded, Some(session), "the session round-trips");
    fs::remove_file(&path).expect("cleanup removes the file");
}

#[test]
fn load_without_a_saved_session_is_none() {
    let store = SessionStore::new(temp_path());
    assert_eq!(
        store.load().expect("a missing file is not an error"),
        None,
        "no file means no session"
    );
}

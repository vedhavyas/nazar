//! Client for the Qubo cloud API: session handling, OTP login, device
//! listing and stream minting.

#![deny(unused_crate_dependencies)]

pub mod auth;
pub mod client;
pub mod control;
pub mod devices;
pub mod error;
pub mod recordings;
pub mod session;
pub mod stream;

pub use auth::{
    AccountProfile, phone_login, refresh_session, request_otp, select_profile, verify_google,
    verify_otp,
};
pub use client::{BASE_URL, QuboClient, ServiceProvider, Url};
pub use control::{Attributes, Shadow, read_shadow, write_service};
pub use devices::{Device, list_devices};
pub use error::QuboError;
pub use recordings::{mint_vod, recorded_days};
pub use session::{Session, SessionStore};
pub use stream::{Mint, Quality, mint_live};

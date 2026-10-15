//! Client for the Qubo cloud API: session handling, OTP login, device
//! listing and stream minting.

#![deny(unused_crate_dependencies)]

pub mod auth;
pub mod client;
pub mod error;
pub mod session;

pub use auth::{request_otp, verify_otp};
pub use client::{BASE_URL, QuboClient, ServiceProvider, Url};
pub use error::QuboError;
pub use session::{Session, SessionStore};

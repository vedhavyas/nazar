//! Phone OTP login, ported from the reference client's `auth.py`.

use serde::Deserialize;
use serde_json::json;

use crate::client::QuboClient;
use crate::error::QuboError;
use crate::session::Session;

/// The label this server shows under in the account's logged-in devices.
const DEVICE_ATTRIBUTE: &str = "Nazar";

/// The family account is an Indian number.
const ISD_CODE: &str = "+91";

/// An account profile from the verify response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    #[serde(alias = "token")]
    access_token: String,
    #[serde(alias = "userUUID", alias = "userId")]
    uuid: String,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OtpRequestResponse {
    uuid: String,
}

/// The verify response is one profile or an array of profiles.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VerifyResponse {
    Profiles(Vec<Profile>),
    Profile(Box<Profile>),
}

/// Send an SMS OTP to `phone`. The login session id the gateway returns is
/// kept on the client until `verify_otp` consumes it.
pub async fn request_otp(client: &QuboClient, phone: &str) -> Result<(), QuboError> {
    let path = format!(
        "/sms/api/v1/sp/{}/user/phone/login",
        client.service_provider()
    );
    let body = json!({
        "phone": phone,
        "isdCode": ISD_CODE,
        "deviceAttribute": DEVICE_ATTRIBUTE,
    });
    let response = client.post_json(&path, &body, None).await?;
    let request: OtpRequestResponse = serde_json::from_value(response)?;
    client.set_pending_login(&request.uuid);
    Ok(())
}

/// Exchange `code` for an account session. A failed attempt keeps the
/// login session, so the code can be retried.
pub async fn verify_otp(
    client: &QuboClient,
    phone: &str,
    code: &str,
) -> Result<Session, QuboError> {
    let login_uuid = client.pending_login().ok_or(QuboError::Auth {
        code: None,
        message: "no OTP was requested for this client".to_owned(),
    })?;

    let path = format!(
        "/sms/api/v1/sp/{}/user/phone/verify",
        client.service_provider()
    );
    let body = json!({
        "phone": phone,
        "isdCode": ISD_CODE,
        "otp": code,
        "deviceAttribute": DEVICE_ATTRIBUTE,
        "uuid": login_uuid,
    });
    let response = client.post_json(&path, &body, None).await?;

    // The account's first profile is the family account.
    let profile = match serde_json::from_value::<VerifyResponse>(response)? {
        VerifyResponse::Profiles(profiles) => profiles.into_iter().next(),
        VerifyResponse::Profile(profile) => Some(*profile),
    };
    let profile = profile.ok_or(QuboError::Auth {
        code: None,
        message: "the login returned no account profile".to_owned(),
    })?;

    client.clear_pending_login();
    Ok(Session {
        access_token: profile.access_token,
        uuid: profile.uuid,
        email: profile.email,
    })
}

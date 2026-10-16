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
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The refresh response (spec §3.2): the access token is required, and the
/// refresh token is optional - the old one is kept when it is absent.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Refreshed {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
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
    client.set_pending_login(&request.uuid, phone);
    Ok(())
}

/// Exchange `code` for an account session. A failed attempt keeps the
/// login session, so the code can be retried.
pub async fn verify_otp(
    client: &QuboClient,
    phone: &str,
    code: &str,
) -> Result<Session, QuboError> {
    let pending = client.pending_login().ok_or(QuboError::Auth {
        code: None,
        message: "no OTP was requested for this client".to_owned(),
    })?;
    if pending.phone != phone {
        // Deliberately silent about the other number: the mismatch is the
        // caller's error to fix, and the message travels to a browser.
        return Err(QuboError::Auth {
            code: None,
            message: "the pending code belongs to another number; request one for this number"
                .to_owned(),
        });
    }

    let path = format!(
        "/sms/api/v1/sp/{}/user/phone/verify",
        client.service_provider()
    );
    let body = json!({
        "phone": phone,
        "isdCode": ISD_CODE,
        "otp": code,
        "deviceAttribute": DEVICE_ATTRIBUTE,
        "uuid": pending.uuid,
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
        refresh_token: profile.refresh_token,
    })
}

/// Exchange a Google ID token for a Qubo session (the app's own `system=GM`
/// v4 login; docs/qubo-google-login.md §3). Its response carries the refresh
/// token the phone route never does - the token comes from the Qubo app's
/// own sign-in, since Google refuses to mint one for anything else.
pub async fn verify_google(client: &QuboClient, id_token: &str) -> Result<Session, QuboError> {
    let path = format!("/sms/api/v4/sp/{}/user/login", client.service_provider());
    let body = json!({
        "accessToken": id_token,
        "username": "",
        "password": "",
        "deviceAttribute": DEVICE_ATTRIBUTE,
    });
    let response = client
        .post_login_json(&path, &[("system", "GM"), ("type", "RELEASE")], &body)
        .await?;
    let profile: Profile = serde_json::from_value(response)?;
    Ok(Session {
        access_token: profile.access_token,
        uuid: profile.uuid,
        email: profile.email,
        refresh_token: profile.refresh_token,
    })
}

/// Spend the stored refresh token for a fresh access token (spec §3.2). The
/// failed access token rides the request, since that is what the gateway
/// knows; a response without a refresh token keeps the stored one.
pub async fn refresh_session(client: &QuboClient, session: &Session) -> Result<Session, QuboError> {
    let refresh_token = session.refresh_token.clone().ok_or(QuboError::Auth {
        code: None,
        message: "no refresh token; log in again".to_owned(),
    })?;
    let path = format!(
        "/sms/api/v1/sp/{}/users/{}/auth/refresh",
        client.service_provider(),
        session.uuid
    );
    let body = json!({
        "accessToken": session.access_token,
        "refreshToken": refresh_token,
    });
    let response = client.post_json(&path, &body, Some(session)).await?;
    let refreshed: Refreshed = serde_json::from_value(response)?;
    Ok(Session {
        access_token: refreshed.access_token,
        uuid: session.uuid.clone(),
        email: session.email.clone(),
        refresh_token: refreshed.refresh_token.or(Some(refresh_token)),
    })
}

//! Phone OTP login, ported from the reference client's `auth.py`.

use serde::Deserialize;
use serde_json::json;

use crate::client::QuboClient;
use crate::error::QuboError;
use crate::session::Session;

/// The label this server shows under in the account's logged-in devices.
const DEVICE_ATTRIBUTE: &str = "Nazar";

/// The server's device identity for the mint's body, in the app's
/// `MANUFACTURER|MODEL|PRODUCT` shape.
const MINT_DEVICE_ATTRIBUTE: &str = "nazar|server|nazar";

/// The same identity as a device name, the app's underscore shape.
const MINT_DEVICE_NAME: &str = "nazar_server";

/// The family account is an Indian number.
const ISD_CODE: &str = "+91";

/// An account profile from a login response. A phone number can map to
/// more than one Qubo account, and the verify response carries them all
/// (spec §3.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    #[serde(alias = "token")]
    pub access_token: String,
    #[serde(alias = "userUUID", alias = "userId")]
    pub uuid: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
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

/// The mint response: the durable pair and the account it belongs to. A
/// field that is absent or empty is not a login.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Minted {
    #[serde(deserialize_with = "non_empty")]
    access_token: String,
    #[serde(deserialize_with = "non_empty")]
    refresh_token: String,
    #[serde(deserialize_with = "non_empty")]
    uuid: String,
}

/// A minted field that must carry something, not merely be present.
fn non_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("empty"));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
struct OtpRequestResponse {
    uuid: String,
}

/// The verify response is one profile or an array of profiles.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VerifyResponse {
    Profiles(Vec<AccountProfile>),
    Profile(Box<AccountProfile>),
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
    // A new code ends the previous attempt, picker included.
    client.clear_pending_profiles();
    Ok(())
}

/// Exchange `code` for the account profiles the number maps to: one for
/// most numbers, several when the phone holds more than one account. A
/// failed attempt keeps the login session, so the code can be retried.
pub async fn verify_otp(
    client: &QuboClient,
    phone: &str,
    code: &str,
) -> Result<Vec<AccountProfile>, QuboError> {
    // Also what a retry sees after a verify whose mint then failed.
    // Whether the gateway replays a spent code on the same login session
    // is unmeasured (measuring it texts a real SMS), so the refusal names
    // both causes rather than guessing.
    let pending = client.pending_login().ok_or(QuboError::Auth {
        code: None,
        message: "no code is pending for this number - it was already verified, or none was requested; request a new code"
            .to_owned(),
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

    let profiles = match serde_json::from_value::<VerifyResponse>(response)? {
        VerifyResponse::Profiles(profiles) => profiles,
        VerifyResponse::Profile(profile) => vec![*profile],
    };
    if profiles.is_empty() {
        return Err(QuboError::Auth {
            code: None,
            message: "the login returned no account profile".to_owned(),
        });
    }

    client.clear_pending_login();
    Ok(profiles)
}

/// The profile the login continues as, by uuid: the picker's answer
/// resolves against the profiles verify returned (spec §3.4).
pub fn select_profile(
    profiles: &[AccountProfile],
    uuid: &str,
) -> Result<AccountProfile, QuboError> {
    profiles
        .iter()
        .find(|profile| profile.uuid == uuid)
        .cloned()
        .ok_or(QuboError::Auth {
            code: None,
            message: "no account profile with that id".to_owned(),
        })
}

/// The profile-select mint: what the app's own pick-then-enter runs,
/// returning the durable pair the verify step never carries (a fresh
/// 1-hour accessToken and a 180-day refreshToken).
pub async fn phone_login(
    client: &QuboClient,
    profile: &AccountProfile,
) -> Result<Session, QuboError> {
    let email = profile
        .email
        .as_deref()
        .filter(|email| !email.is_empty())
        .ok_or(QuboError::Auth {
            code: None,
            message: "the account profile carries no email to mint with".to_owned(),
        })?;
    let path = format!(
        "/sms/api/v1/sp/{}/user/emailprofile/login",
        client.service_provider()
    );
    let body = json!({
        "deviceAttribute": MINT_DEVICE_ATTRIBUTE,
        "username": email,
    });
    let response = client
        .post_login_json(
            &path,
            &[],
            &[
                ("App-Type", "HOME"),
                ("Login-Device-Name", MINT_DEVICE_NAME),
                ("Subscriber-Key", profile.access_token.as_str()),
                ("Token-Type", "USER"),
                ("User-UUID", profile.uuid.as_str()),
            ],
            &body,
        )
        .await?;
    let minted: Minted = serde_json::from_value(response)?;
    Ok(Session {
        access_token: minted.access_token,
        uuid: minted.uuid,
        email: profile.email.clone(),
        refresh_token: Some(minted.refresh_token),
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
        .post_login_json(&path, &[("system", "GM"), ("type", "RELEASE")], &[], &body)
        .await?;
    let profile: AccountProfile = serde_json::from_value(response)?;
    // An empty token is no token: it would only be a refresh attempt the
    // gateway refuses later.
    let refresh_token = profile.refresh_token.filter(|token| !token.is_empty());
    if refresh_token.is_none() {
        // Warn-only: a hard refusal would turn a working hour into a 502
        // if the shape ever changes; the session simply will not renew
        // silently.
        tracing::warn!("the GM login answered no usable refresh token");
    }
    Ok(Session {
        access_token: profile.access_token,
        uuid: profile.uuid,
        email: profile.email,
        refresh_token,
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

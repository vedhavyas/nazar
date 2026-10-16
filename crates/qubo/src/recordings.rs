//! Recording browsing: the cloud's calendar of recorded days, and the VOD
//! session a clip's instant buys.

use serde::Deserialize;
use serde_json::json;

use crate::client::QuboClient;
use crate::devices::Device;
use crate::error::QuboError;
use crate::session::Session;
use crate::stream::Mint;

/// The days `device` holds recordings for, between `from_ms` and `to_ms`
/// (epoch milliseconds), as `DD-MM-YYYY` strings, oldest first.
///
/// The cloud answers with a bare array in no particular order (captured
/// 2026-10-16, docs/qubo-vod-verification.md), so the sorting is this
/// function's.
pub async fn recorded_days(
    client: &QuboClient,
    session: &Session,
    device: &Device,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<String>, QuboError> {
    let path = format!(
        "/content-manager/api/v1/sp/{}/unit/{}/device/{}/calendar",
        client.service_provider(),
        device.unit_id,
        device.device_id
    );
    let from = from_ms.to_string();
    let to = to_ms.to_string();
    let query = [
        ("fromTimeStamp", from.as_str()),
        ("toTimeStamp", to.as_str()),
        ("timezone", "Asia/Kolkata"),
    ];
    let response = client.get_json_query(&path, &query, Some(session)).await?;
    let mut days: Vec<String> = serde_json::from_value(response)?;
    days.sort_by_key(|day| day_key(day));
    Ok(days)
}

/// The v2 VOD mint's response: `streamURL` is spelled with the capital
/// URL, and the session rides `appSessionId`, not the live route's
/// `sessionId` (captured 2026-10-16, docs/qubo-vod-verification.md).
#[derive(Debug, Deserialize)]
struct VodMintResponse {
    #[serde(rename = "streamURL")]
    stream_url: String,
    #[serde(rename = "appSessionId")]
    app_session_id: String,
}

/// Mint a playback session for `device` at `timestamp_ms` (epoch
/// milliseconds). The POST is what works; no PUT fallback exists.
pub async fn mint_vod(
    client: &QuboClient,
    session: &Session,
    device: &Device,
    timestamp_ms: i64,
) -> Result<Mint, QuboError> {
    let path = format!(
        "/stream-manager/api/v2/sp/{}/stream/unit/{}/device/{}",
        client.service_provider(),
        device.unit_id,
        device.device_id
    );
    let timestamp = timestamp_ms.to_string();
    let query = [
        ("timestamp", timestamp.as_str()),
        ("type", "high"),
        ("streamSourceCamera", "primary"),
        ("singleRecord", "0"),
    ];
    let body = json!({ "userAppID": client.device_id() });
    let response = client
        .post_json_query(&path, &query, &body, Some(session))
        .await?;
    let mint: VodMintResponse = serde_json::from_value(response)?;
    Ok(Mint {
        url: mint.stream_url,
        session_id: mint.app_session_id,
    })
}

/// Sort order for the calendar's `DD-MM-YYYY` strings. The key is (year,
/// month, day): sorting the strings as written orders a 30-day window by
/// day-of-month, interleaving the months the window crosses. A string that
/// is not a day sorts first, where it stays visible rather than being
/// dropped.
fn day_key(day: &str) -> Option<(i64, i64, i64)> {
    let mut parts = day.split('-');
    let day = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let year = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((year, month, day))
}

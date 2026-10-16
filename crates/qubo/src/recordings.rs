//! Recording browsing: the cloud's calendar of recorded days.

use crate::client::QuboClient;
use crate::devices::Device;
use crate::error::QuboError;
use crate::session::Session;

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

/// Sort order for the calendar's `DD-MM-YYYY` strings. A string that is
/// not a day sorts first, where it stays visible rather than being dropped.
fn day_key(day: &str) -> Option<(i64, i64, i64)> {
    let mut parts = day.split('-');
    let key = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(key)
}

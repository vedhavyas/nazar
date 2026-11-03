//! The client's error model.

use reqwest::StatusCode;
use serde_json::Value;

/// Everything that can go wrong on the way to the Qubo cloud.
#[derive(Debug, thiserror::Error)]
pub enum QuboError {
    /// The request never produced a usable answer: a transport failure,
    /// or a non-2xx response with no error envelope in the body.
    #[error("http error: {message}")]
    Http {
        status: Option<u16>,
        message: String,
    },

    /// A non-2xx response carrying the API's `{code, message}` envelope.
    /// The status is kept: a 4xx refusal and a 5xx outage are different
    /// events to every caller that routes on them.
    #[error("api error {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },

    /// 401/403 - the session is missing, expired or refused.
    #[error("auth error: {message}")]
    Auth {
        code: Option<String>,
        message: String,
    },

    /// A 2xx body that is not the expected JSON.
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),

    /// Local IO while reading or writing the session file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for QuboError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http {
            status: err.status().map(|status| status.as_u16()),
            message: transport_message(&err),
        }
    }
}

/// A transport failure's words with the URL scrubbed: its class, then the
/// cause chain. reqwest's own Display echoes the URL, which can carry an
/// account id or a token, and it prints one sentence for very different
/// failures - the class and the chain are what tell them apart.
fn transport_message(err: &reqwest::Error) -> String {
    let mut text = String::from(if err.is_timeout() {
        "timeout"
    } else if err.is_dns() {
        "dns"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() || err.is_decode() {
        "body"
    } else {
        "request"
    });
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Map a non-2xx response to the right variant, reading the API's error
/// envelope when the body carries one.
pub(crate) fn error_for_status(status: StatusCode, body: &str) -> QuboError {
    let envelope = serde_json::from_str::<Value>(body).ok().and_then(|value| {
        let code = value.get("code")?.as_str()?.to_owned();
        Some((code, value.get("message").and_then(message_text)))
    });

    let rejected = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);

    if let Some((code, message)) = envelope {
        let message = message.unwrap_or_else(|| status.to_string());
        return if rejected {
            QuboError::Auth {
                code: Some(code),
                message,
            }
        } else {
            QuboError::Api {
                status: status.as_u16(),
                code,
                message,
            }
        };
    }

    let message = body_snippet(body).unwrap_or_else(|| status.to_string());
    if rejected {
        QuboError::Auth {
            code: None,
            message,
        }
    } else {
        QuboError::Http {
            status: Some(status.as_u16()),
            message,
        }
    }
}

/// The envelope's `message` is a string on some routes and an array of
/// strings on others (the 401 capture carries an array).
fn message_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

fn body_snippet(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(body.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_envelope_carries_its_status_and_code() -> Result<(), String> {
        let err = error_for_status(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"code":"RATE","message":"slow down"}"#,
        );
        match err {
            QuboError::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 429);
                assert_eq!(code, "RATE");
                assert_eq!(message, "slow down");
                Ok(())
            }
            other => Err(format!("expected Api, got {other:?}")),
        }
    }
}

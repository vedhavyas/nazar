//! Credential redaction for the server's outward text: HTTP diagnostics,
//! API errors and log lines. A minted stream URL carries its session in
//! the query, and an engine or cloud error quotes the URL it failed on.

/// What a masked value is replaced with. Both scanners pass the marker
/// over whole, so a value this module already masked is never re-cut at
/// the marker's own `>`.
const REDACTED: &str = "<redacted>";

/// Schemes whose query string can carry a minted stream's credentials.
const URL_SCHEMES: [&str; 6] = [
    "https://", "http://", "rtsps://", "rtsp://", "wss://", "ws://",
];

/// A URL keeps its endpoint and loses its query, where a mint's
/// credentials ride. The scheme is matched in any case and kept as found.
pub(crate) fn strip_url_queries(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    loop {
        let next = URL_SCHEMES
            .iter()
            .filter_map(|scheme| lower[at..].find(scheme).map(|off| (at + off, *scheme)))
            .min_by_key(|(start, _)| *start);
        let Some((start, scheme)) = next else {
            out.push_str(&text[at..]);
            return out;
        };
        out.push_str(&text[at..start]);
        let tail = &text[start + scheme.len()..];
        let end = url_end(tail);
        let url = &tail[..end];
        out.push_str(&text[start..start + scheme.len()]);
        match url.find('?') {
            Some(query) => {
                out.push_str(&url[..query]);
                out.push('?');
                out.push_str(REDACTED);
            }
            None => out.push_str(url),
        }
        at = start + scheme.len() + end;
    }
}

/// How far a URL runs: to whitespace, or the punctuation that ends a
/// quoted URL. A marker this module wrote is passed over whole - the
/// supervisor redacts a reason before the HTTP surface strips it, and the
/// scan must still reach the URL's real end.
fn url_end(tail: &str) -> usize {
    let bytes = tail.as_bytes();
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        if bytes[i..].starts_with(REDACTED.as_bytes()) {
            i += REDACTED.len();
            continue;
        }
        if byte.is_ascii_whitespace() || matches!(byte, b')' | b']' | b'"' | b'\'' | b'>') {
            break;
        }
        i += 1;
    }
    i
}

/// Parameter names a mint's credentials ride under, stored in the cloud's
/// own lowercase spelling and matched case-insensitively.
const CREDENTIAL_KEYS: [&str; 5] = [
    "wowzatoken",
    "sessionid",
    "useruuid",
    "camsessionid",
    "token",
];

/// Every credential parameter's value is masked in place. The separator
/// is `=`, a colon (only where whitespace, a quote or a backslash follows)
/// or a percent-encoded one, and the value may be bare, quoted or escaped.
pub(crate) fn redact_credentials(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    loop {
        let next = CREDENTIAL_KEYS
            .iter()
            .filter_map(|key| lower[at..].find(key).map(|off| (at + off, *key)))
            .min_by_key(|(start, _)| *start);
        let Some((start, key)) = next else {
            out.push_str(&text[at..]);
            return out;
        };
        let after_key = start + key.len();
        // The key is part of a parameter name; a suffix on it
        // (`wowzatokenhash`) is still the credential's own parameter.
        let mut name_end = after_key;
        while let Some(&byte) = bytes.get(name_end) {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') {
                name_end += 1;
            } else {
                break;
            }
        }
        let mut sep_at = name_end;
        while bytes.get(sep_at).is_some_and(u8::is_ascii_whitespace) {
            sep_at += 1;
        }
        let sep = match bytes.get(sep_at) {
            Some(b'=') => Some(1),
            // A colon is a separator only where a value clearly follows it,
            // so prose like `token:type` is left alone.
            Some(b':')
                if bytes.get(sep_at + 1).is_some_and(|byte| {
                    byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'\\')
                }) =>
            {
                Some(1)
            }
            // A JSON body quotes its key: `{"token": "secret"}`.
            Some(b'"' | b'\'')
                if bytes.get(sep_at + 1) == Some(&b':')
                    && bytes.get(sep_at + 2).is_some_and(|byte| {
                        byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'\\')
                    }) =>
            {
                Some(2)
            }
            // A JSON body escaped inside a string: `{\"token\":\"secret\"}`.
            Some(b'\\')
                if matches!(bytes.get(sep_at + 1), Some(b'"' | b'\''))
                    && bytes.get(sep_at + 2) == Some(&b':') =>
            {
                Some(3)
            }
            _ => encoded_len(bytes, sep_at, b'3', b'd').or_else(|| encoded_json_sep(bytes, sep_at)),
        };
        let Some(sep) = sep else {
            // The whole name is emitted with no re-scan of its own suffix,
            // which would make a long non-parameter word quadratic.
            out.push_str(&text[at..name_end]);
            at = name_end;
            continue;
        };
        let mut value_start = sep_at + sep;
        while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        // A quote right after the separator opens the value rather than
        // ending it: `token="secret"` must not slip the secret past the
        // quote-as-terminator rule. A backslash belongs to the opener too,
        // for `token=\"secret\"` in a quoted body.
        let opener = match (bytes.get(value_start), bytes.get(value_start + 1)) {
            (Some(b'\\'), Some(&quote @ (b'"' | b'\''))) => {
                value_start += 2;
                Some((quote, true))
            }
            (Some(&quote @ (b'"' | b'\'')), _) => {
                value_start += 1;
                Some((quote, false))
            }
            _ => None,
        };
        let end = match opener {
            Some((quote, escaped)) => quoted_end(bytes, value_start, quote, escaped)
                .unwrap_or_else(|| value_end(bytes, value_start)),
            None => value_end(bytes, value_start),
        };
        out.push_str(&text[at..value_start]);
        out.push_str(REDACTED);
        at = value_start + end;
    }
}

/// The length of a percent-encoded JSON key's closer and colon (`%22%3A`
/// or `%27%3A`), as a nested encoding leaves them.
fn encoded_json_sep(bytes: &[u8], at: usize) -> Option<usize> {
    let quote =
        encoded_len(bytes, at, b'2', b'2').or_else(|| encoded_len(bytes, at, b'2', b'7'))?;
    let colon = encoded_len(bytes, at + quote, b'3', b'a')?;
    Some(quote + colon)
}

/// The length of an encoded `=` or `&`, accepting either case and any
/// number of `%25` escapes for the percent itself (`%3D`, `%3d`, `%253D`,
/// `%25253D`).
fn encoded_len(bytes: &[u8], at: usize, lead: u8, tail: u8) -> Option<usize> {
    if bytes.get(at) != Some(&b'%') {
        return None;
    }
    let mut i = at + 1;
    while bytes.len() >= i + 2 && &bytes[i..i + 2] == b"25" {
        i += 2;
    }
    if bytes.len() >= i + 2 && bytes[i] == lead && bytes[i + 1].eq_ignore_ascii_case(&tail) {
        Some(i + 2 - at)
    } else {
        None
    }
}

/// How far a quoted value runs: to its closing quote, or to the closing
/// `\"` pair of an escaped one. `None` when the quote never closes.
fn quoted_end(bytes: &[u8], at: usize, quote: u8, escaped: bool) -> Option<usize> {
    if escaped {
        bytes[at..]
            .windows(2)
            .position(|pair| pair == [b'\\', quote])
    } else {
        bytes[at..].iter().position(|&byte| byte == quote)
    }
}

/// How far a credential's value runs before a separator: raw or encoded
/// `&` and `#`, whitespace, or the punctuation that ends a quoted URL. The
/// `#` matters twice over: the source rides as `?token=..#input=nazar..`,
/// and without it the whole tail of the message would be swallowed.
fn value_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at;
    while let Some(&byte) = bytes.get(i) {
        if bytes[i..].starts_with(REDACTED.as_bytes()) {
            i += REDACTED.len();
            continue;
        }
        let ends = byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'&' | b')' | b']' | b'"' | b'\'' | b'#' | b'\\' | b'>'
            )
            || (byte == b'%'
                && (encoded_len(bytes, i, b'2', b'6').is_some()
                    || encoded_len(bytes, i, b'2', b'3').is_some()));
        if ends {
            break;
        }
        i += 1;
    }
    i - at
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn a_value_is_masked_in_every_shape_it_arrives() {
        let cases = [
            ("?token=secret&next=1", "?token=<redacted>&next=1"),
            ("?token=#input=nazar", "?token=<redacted>#input=nazar"),
            ("?token=a%23b", "?token=<redacted>%23b"),
            ("?token=&next=1", "?token=<redacted>&next=1"),
            ("camSessionId%3Dabc", "camSessionId%3D<redacted>"),
            ("wowzaToken%25253Dabc", "wowzaToken%25253D<redacted>"),
            ("token=\"a secret\"", "token=\"<redacted>\""),
            ("token='a secret'", "token='<redacted>'"),
            ("token=\"unclosed", "token=\"<redacted>"),
            ("token=\"\"", "token=\"<redacted>\""),
            ("token= \"a secret\"", "token= \"<redacted>\""),
            ("token = S3CR3T", "token = <redacted>"),
            ("token: S3CR3T", "token: <redacted>"),
            ("token=\\\"S3CR3T\\\"", "token=\\\"<redacted>\\\""),
            ("{\"token\": \"secret\"}", "{\"token\": \"<redacted>\"}"),
            (
                "{\\\"token\\\":\\\"secret\\\"}",
                "{\\\"token\\\":\\\"<redacted>\\\"}",
            ),
            (
                "%22token%22%3A%22secret%22%26next%3D1",
                "%22token%22%3A<redacted>%26next%3D1",
            ),
            ("token= spaced", "token= <redacted>"),
            ("token=café", "token=<redacted>"),
        ];
        for (raw, expected) in cases {
            assert_eq!(redact_credentials(raw), expected, "input: {raw}");
        }
    }

    #[test]
    fn ordinary_words_are_left_alone() {
        let cases = [
            "the token is missing",
            "a tokenizable field",
            "the café camera did not answer",
            "sessionIdle is a different word",
        ];
        for text in cases {
            assert_eq!(redact_credentials(text), text, "input: {text}");
        }
    }

    #[test]
    fn a_redacted_reason_is_stable_under_a_second_pass() {
        let raw = "invalid source: src=rtsps://wowza.example/c?wowzaToken%253Dsek&sessionId=sid#input=nazar";
        let once = redact_credentials(raw);
        assert_eq!(
            once,
            "invalid source: src=rtsps://wowza.example/c?wowzaToken%253D<redacted>&sessionId=<redacted>#input=nazar"
        );
        assert_eq!(
            redact_credentials(&once),
            once,
            "a second pass changes nothing"
        );
    }

    #[test]
    fn a_url_keeps_its_endpoint_and_loses_its_query() {
        let cases = [
            (
                "url (https://cloud.example/live/cam?sessionId=sid&unitUUID=unit-7)",
                "url (https://cloud.example/live/cam?<redacted>)",
            ),
            (
                "url (https://cloud.example/live/cam?sessionId=<redacted>&unitUUID=unit-7)",
                "url (https://cloud.example/live/cam?<redacted>)",
            ),
            (
                "URL (HTTPS://cloud.example/live/cam?sessionId=sid)",
                "URL (HTTPS://cloud.example/live/cam?<redacted>)",
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(strip_url_queries(raw), expected, "input: {raw}");
        }
    }
}

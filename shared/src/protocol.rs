use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Invalid request format")]
    InvalidRequest,
    #[error("Invalid response format")]
    InvalidResponse,
    #[error("URL too long")]
    UrlTooLong,
    #[error("Payload too large")]
    PayloadTooLarge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Get {
        url: String,
        if_none_match: Option<String>,
    },
    Post { url: String, body: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0x00,
    Err = 0x01,
    Blocked = 0x02,
    NotModified = 0x03,
}

impl TryFrom<u8> for Status {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0x00 => Ok(Status::Ok),
            0x01 => Ok(Status::Err),
            0x02 => Ok(Status::Blocked),
            0x03 => Ok(Status::NotModified),
            _ => Err(ProtocolError::InvalidResponse),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: Status,
    pub etag: String,
    pub max_age: i32,
    pub payload: Vec<u8>,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Get { url, if_none_match: None } => {
                let mut data = Vec::with_capacity(5 + url.len());
                data.extend_from_slice(b"GET ");
                data.extend_from_slice(url.as_bytes());
                data.push(b'\n');
                data
            }
            Request::Get { url, if_none_match: Some(etag) } => {
                let mut data = Vec::with_capacity(4 + url.len() + 16 + etag.len() + 1);
                data.extend_from_slice(b"GET ");
                data.extend_from_slice(url.as_bytes());
                data.extend_from_slice(b" IF-NONE-MATCH ");
                data.extend_from_slice(etag.as_bytes());
                data.push(b'\n');
                data
            }
            Request::Post { url, body } => {
                let body_len = body.len() as u32;
                let mut data = Vec::with_capacity(5 + url.len() + 4 + body.len());
                data.extend_from_slice(b"POST ");
                data.extend_from_slice(url.as_bytes());
                data.push(b'\n');
                data.extend_from_slice(&body_len.to_be_bytes());
                data.extend_from_slice(body);
                data
            }
        }
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.is_empty() {
            return Err(ProtocolError::InvalidRequest);
        }
        if data.starts_with(b"GET ") {
            let line_end = data[4..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(ProtocolError::InvalidRequest)?;
            let line = std::str::from_utf8(&data[4..4 + line_end])
                .map_err(|_| ProtocolError::InvalidRequest)?;
            // Split on the sentinel token; the URL itself must not contain " IF-NONE-MATCH ".
            if let Some((url, etag)) = line.split_once(" IF-NONE-MATCH ") {
                Ok(Request::Get {
                    url: url.to_string(),
                    if_none_match: Some(etag.to_string()),
                })
            } else {
                Ok(Request::Get {
                    url: line.to_string(),
                    if_none_match: None,
                })
            }
        } else if data.starts_with(b"POST ") {
            let url_end = data[5..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(ProtocolError::InvalidRequest)?;
            let url = std::str::from_utf8(&data[5..5 + url_end])
                .map_err(|_| ProtocolError::InvalidRequest)?
                .to_string();
            let body_start = 5 + url_end + 1;
            if data.len() < body_start + 4 {
                return Err(ProtocolError::InvalidRequest);
            }
            let body_len = u32::from_be_bytes([
                data[body_start],
                data[body_start + 1],
                data[body_start + 2],
                data[body_start + 3],
            ]) as usize;
            if data.len() < body_start + 4 + body_len {
                return Err(ProtocolError::InvalidRequest);
            }
            let body = data[body_start + 4..body_start + 4 + body_len].to_vec();
            Ok(Request::Post { url, body })
        } else {
            Err(ProtocolError::InvalidRequest)
        }
    }
}

/// On-the-wire text framing for a Response, designed to survive LinBPQ's
/// TELNET/HOST 0 S mode which strips NUL bytes and does CRLF translation.
///
/// Format (all printable ASCII, LF-terminated header, base64 payload,
/// LF-terminated):
///
///   `RESP<status_digit> <base64_len> <etag> <max_age>\n<base64_payload>\n`
///
/// * `RESP` — 4-byte magic so the receiver can resync on the frame start
///   even if leading bytes (banners, prompt echoes) precede the response.
/// * `<status_digit>` — one ASCII digit: `0` = Ok, `1` = Err, `2` = Blocked, `3` = NotModified.
/// * `<base64_len>` — length of the base64-encoded payload, ASCII decimal.
/// * `<etag>` — the ETag value (16 chars or `-` if not applicable).
/// * `<max_age>` — cache control max-age value, signed ASCII decimal.
/// * The payload is base64 (RFC 4648 standard alphabet + padding); it never
///   contains NUL, CR, LF, IAC, or other bytes the telnet driver would molest.
///
/// # 47 CFR Part 97 note
///
/// The two transformations applied to the response body — RFC 7932 brotli
/// compression and RFC 4648 base64 encoding — are both open, publicly
/// documented, non-proprietary techniques. Neither is intended to obscure
/// meaning; both are here purely for bandwidth (brotli) and telnet-layer
/// byte-transparency (base64). Any listener who reads this comment can
/// recover the transmitted HTML with off-the-shelf tools (`openssl base64
/// -d | brotli --decompress`). See the "Wire Format & Part 97 Compliance"
/// section of README.md for the full rationale.
impl Response {
    pub const MAGIC: &'static [u8] = b"RESP";

    pub fn encode(&self) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let encoded_payload = STANDARD.encode(&self.payload);
        let status_digit = match self.status {
            Status::Ok => '0',
            Status::Err => '1',
            Status::Blocked => '2',
            Status::NotModified => '3',
        };
        let mut data = Vec::with_capacity(
            Self::MAGIC.len() + 32 + self.etag.len() + encoded_payload.len() + 2,
        );
        data.extend_from_slice(Self::MAGIC);
        data.push(status_digit as u8);
        data.push(b' ');
        data.extend_from_slice(encoded_payload.len().to_string().as_bytes());
        data.push(b' ');
        data.extend_from_slice(self.etag.as_bytes());
        data.push(b' ');
        data.extend_from_slice(self.max_age.to_string().as_bytes());
        data.push(b'\n');
        data.extend_from_slice(encoded_payload.as_bytes());
        data.push(b'\n');
        data
    }

    pub fn decode_header(
        data: &[u8],
    ) -> Result<Option<(Status, u32, String, i32, usize)>, ProtocolError> {
        let magic_pos = match data
            .windows(Self::MAGIC.len())
            .position(|w| w == Self::MAGIC)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        let after_magic = magic_pos + Self::MAGIC.len();
        if data.len() < after_magic + 2 {
            return Ok(None);
        }
        let status = match data[after_magic] {
            b'0' => Status::Ok,
            b'1' => Status::Err,
            b'2' => Status::Blocked,
            b'3' => Status::NotModified,
            _ => return Err(ProtocolError::InvalidResponse),
        };
        if data[after_magic + 1] != b' ' {
            return Err(ProtocolError::InvalidResponse);
        }
        let fields_start = after_magic + 2;
        let nl_offset = match data[fields_start..].iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(o) => o,
            None => return Ok(None),
        };
        let header_line = std::str::from_utf8(&data[fields_start..fields_start + nl_offset])
            .map_err(|_| ProtocolError::InvalidResponse)?;
        let mut parts = header_line.split(' ');
        let len_str = parts.next().ok_or(ProtocolError::InvalidResponse)?;
        let etag = parts.next().ok_or(ProtocolError::InvalidResponse)?.to_string();
        let valid_etag = etag == "-"
            || (etag.len() == 16
                && etag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        if !valid_etag {
            return Err(ProtocolError::InvalidResponse);
        }
        let max_age_str = parts.next().ok_or(ProtocolError::InvalidResponse)?;
        if parts.next().is_some() {
            return Err(ProtocolError::InvalidResponse);
        }
        let payload_len: u32 = len_str.parse().map_err(|_| ProtocolError::InvalidResponse)?;
        let max_age: i32 = max_age_str.parse().map_err(|_| ProtocolError::InvalidResponse)?;
        let header_end = fields_start + nl_offset + 1;
        Ok(Some((status, payload_len, etag, max_age, header_end)))
    }

    /// Decode the base64 payload bytes back to the raw payload.
    pub fn decode_payload(base64_bytes: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD
            .decode(base64_bytes)
            .map_err(|_| ProtocolError::InvalidResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_request_roundtrip() {
        let req = Request::Get {
            url: "https://example.com".to_string(),
            if_none_match: None,
        };
        let encoded = req.encode();
        let decoded = Request::decode(&encoded).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_post_request_roundtrip() {
        let req = Request::Post {
            url: "https://example.com/form".to_string(),
            body: b"field1=value1&field2=value2".to_vec(),
        };
        let encoded = req.encode();
        let decoded = Request::decode(&encoded).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_get_request_with_if_none_match_roundtrip() {
        let req = Request::Get {
            url: "https://example.com".to_string(),
            if_none_match: Some("aBcDeFgHiJkLmNoP".to_string()),
        };
        let encoded = req.encode();
        assert!(encoded.starts_with(b"GET https://example.com IF-NONE-MATCH aBcDeFgHiJkLmNoP\n"));
        let decoded = Request::decode(&encoded).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_get_request_without_if_none_match_roundtrip() {
        let req = Request::Get {
            url: "https://example.com".to_string(),
            if_none_match: None,
        };
        let encoded = req.encode();
        assert_eq!(encoded, b"GET https://example.com\n");
        let decoded = Request::decode(&encoded).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_response_encode_decode() {
        let resp = Response {
            status: Status::Ok,
            etag: "-".to_string(),
            max_age: -1,
            payload: b"test payload".to_vec(),
        };
        let encoded = resp.encode();

        // Every byte on the wire must be a printable/whitespace character —
        // no NUL, no CR, no IAC — otherwise LinBPQ's TELNET driver mangles it.
        for &b in &encoded {
            assert!(
                b == b'\n' || (0x20..=0x7e).contains(&b),
                "wire byte 0x{:02x} is not telnet-safe",
                b,
            );
        }

        let (status, b64_len, _etag, _max_age, header_end) =
            Response::decode_header(&encoded).unwrap().unwrap();
        assert_eq!(status, Status::Ok);
        let payload = Response::decode_payload(
            &encoded[header_end..header_end + b64_len as usize],
        )
        .unwrap();
        assert_eq!(payload, b"test payload");
    }

    #[test]
    fn test_response_resyncs_past_garbage() {
        let resp = Response {
            status: Status::Err,
            etag: "-".to_string(),
            max_age: -1,
            payload: b"nope".to_vec(),
        };
        let mut with_prefix = b"junk banner text\r".to_vec();
        with_prefix.extend_from_slice(&resp.encode());

        let (status, b64_len, _etag, _max_age, header_end) =
            Response::decode_header(&with_prefix).unwrap().unwrap();
        assert_eq!(status, Status::Err);
        let payload = Response::decode_payload(
            &with_prefix[header_end..header_end + b64_len as usize],
        )
        .unwrap();
        assert_eq!(payload, b"nope");
    }

    #[test]
    fn test_response_no_binary_bytes_in_wire() {
        // A payload full of bytes LinBPQ historically drops (NUL) or interprets
        // (CR, LF, IAC) must not appear raw on the wire — base64 keeps us safe.
        let payload: Vec<u8> = (0u8..=255).collect();
        let encoded = Response {
            status: Status::Ok,
            etag: "-".to_string(),
            max_age: -1,
            payload: payload.clone(),
        }
        .encode();

        for &b in &encoded {
            assert!(b != 0x00 && b != b'\r' && b != 0xff);
        }

        let (_, b64_len, _etag, _max_age, header_end) =
            Response::decode_header(&encoded).unwrap().unwrap();
        let round_tripped = Response::decode_payload(
            &encoded[header_end..header_end + b64_len as usize],
        )
        .unwrap();
        assert_eq!(round_tripped, payload);
    }

    #[test]
    fn test_status_codes() {
        assert_eq!(Status::try_from(0x00).unwrap(), Status::Ok);
        assert_eq!(Status::try_from(0x01).unwrap(), Status::Err);
        assert_eq!(Status::try_from(0x02).unwrap(), Status::Blocked);
        assert_eq!(Status::try_from(0x03).unwrap(), Status::NotModified);
        assert!(Status::try_from(0x04).is_err());
    }

    #[test]
    fn decode_header_rejects_malformed_etag() {
        // 17-char etag — too long.
        let bad = b"RESP0 3 aBcDeFgHiJkLmNoPq 0\nabc\n";
        assert!(matches!(
            Response::decode_header(bad),
            Err(ProtocolError::InvalidResponse)
        ));
        // Etag with a byte outside the base64url alphabet (`+`).
        let bad = b"RESP0 3 aBcDeFgHiJk+mNoP 0\nabc\n";
        assert!(matches!(
            Response::decode_header(bad),
            Err(ProtocolError::InvalidResponse)
        ));
        // Placeholder "-" is still accepted.
        let ok = b"RESP1 3 - -1\nabc\n";
        assert!(Response::decode_header(ok).is_ok());
    }

    #[test]
    fn test_response_with_etag_and_max_age_roundtrip() {
        let resp = Response {
            status: Status::Ok,
            etag: "aBcDeFgHiJkLmNoP".to_string(),
            max_age: 3600,
            payload: b"body".to_vec(),
        };
        let encoded = resp.encode();
        assert!(encoded.starts_with(b"RESP0 "));
        let (status, b64_len, etag, max_age, header_end) =
            Response::decode_header(&encoded).unwrap().unwrap();
        assert_eq!(status, Status::Ok);
        assert_eq!(etag, "aBcDeFgHiJkLmNoP");
        assert_eq!(max_age, 3600);
        let payload = Response::decode_payload(&encoded[header_end..header_end + b64_len as usize]).unwrap();
        assert_eq!(payload, b"body");
    }

    #[test]
    fn test_not_modified_status_roundtrip() {
        let resp = Response {
            status: Status::NotModified,
            etag: "aBcDeFgHiJkLmNoP".to_string(),
            max_age: 3600,
            payload: Vec::new(),
        };
        let encoded = resp.encode();
        assert!(encoded.starts_with(b"RESP3 0 aBcDeFgHiJkLmNoP 3600\n"));
        let (status, b64_len, etag, max_age, _) =
            Response::decode_header(&encoded).unwrap().unwrap();
        assert_eq!(status, Status::NotModified);
        assert_eq!(b64_len, 0);
        assert_eq!(etag, "aBcDeFgHiJkLmNoP");
        assert_eq!(max_age, 3600);
    }

    #[test]
    fn test_response_negative_max_age_roundtrip() {
        let resp = Response {
            status: Status::Ok,
            etag: "-".to_string(),
            max_age: -1,
            payload: b"x".to_vec(),
        };
        let encoded = resp.encode();
        let (_status, _b64_len, etag, max_age, _header_end) = Response::decode_header(&encoded).unwrap().unwrap();
        assert_eq!(etag, "-");
        assert_eq!(max_age, -1);
    }

    #[test]
    fn test_wire_bytes_still_telnet_safe_with_new_fields() {
        let payload: Vec<u8> = (0u8..=255).collect();
        let encoded = Response {
            status: Status::Ok,
            etag: "aBcDeFgHiJkLmNoP".to_string(),
            max_age: 42,
            payload,
        }
        .encode();
        for &b in &encoded {
            assert!(b == b'\n' || (0x20..=0x7e).contains(&b),
                    "wire byte 0x{:02x} not telnet-safe", b);
        }
    }
}

/// Compute the wire etag for a sanitized HTML body.
///
/// Definition: base64url-nopad(sha256(html_utf8_bytes)[..12]) → exactly 16
/// ASCII chars. Base64url is the RFC 4648 "-_" alphabet.
pub fn sanitized_html_etag(html: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest, Sha256};

    let hash = Sha256::digest(html.as_bytes());
    URL_SAFE_NO_PAD.encode(&hash[..12])
}

#[cfg(test)]
mod etag_tests {
    use super::*;

    #[test]
    fn etag_is_sixteen_chars() {
        assert_eq!(sanitized_html_etag("hello").len(), 16);
        assert_eq!(sanitized_html_etag("").len(), 16);
    }

    #[test]
    fn etag_is_deterministic() {
        assert_eq!(sanitized_html_etag("abc"), sanitized_html_etag("abc"));
    }

    #[test]
    fn etag_differs_on_content_change() {
        assert_ne!(sanitized_html_etag("abc"), sanitized_html_etag("abd"));
    }

    #[test]
    fn etag_uses_base64url_alphabet() {
        let e = sanitized_html_etag("hello world");
        for c in e.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "unexpected char {:?} in etag {}", c, e,
            );
        }
    }
}

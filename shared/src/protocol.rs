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
    Get { url: String },
    Post { url: String, body: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0x00,
    Err = 0x01,
    Blocked = 0x02,
}

impl TryFrom<u8> for Status {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0x00 => Ok(Status::Ok),
            0x01 => Ok(Status::Err),
            0x02 => Ok(Status::Blocked),
            _ => Err(ProtocolError::InvalidResponse),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: Status,
    pub payload: Vec<u8>,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Get { url } => {
                let mut data = Vec::with_capacity(4 + url.len());
                data.extend_from_slice(b"GET ");
                data.extend_from_slice(url.as_bytes());
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
            let url_end = data[4..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(ProtocolError::InvalidRequest)?;
            let url = std::str::from_utf8(&data[4..4 + url_end])
                .map_err(|_| ProtocolError::InvalidRequest)?
                .to_string();
            Ok(Request::Get { url })
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
///   `RESP<status_digit> <base64_len>\n<base64_payload>\n`
///
/// * `RESP` — 4-byte magic so the receiver can resync on the frame start
///   even if leading bytes (banners, prompt echoes) precede the response.
/// * `<status_digit>` — one ASCII digit: `0` = Ok, `1` = Err, `2` = Blocked.
/// * `<base64_len>` — length of the base64-encoded payload, ASCII decimal.
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
        };
        let mut data = Vec::with_capacity(
            Self::MAGIC.len() + 16 + encoded_payload.len() + 2,
        );
        data.extend_from_slice(Self::MAGIC);
        data.push(status_digit as u8);
        data.push(b' ');
        data.extend_from_slice(encoded_payload.len().to_string().as_bytes());
        data.push(b'\n');
        data.extend_from_slice(encoded_payload.as_bytes());
        data.push(b'\n');
        data
    }

    /// Try to parse the response header out of `data`, tolerating any leading
    /// bytes before the `RESP` magic. Returns `(status, base64_payload_len,
    /// header_end_offset)` where `header_end_offset` is the index just past
    /// the trailing `\n` of the header line — the base64 payload begins
    /// there. If no complete header is present yet, returns `Ok(None)` so the
    /// caller can keep reading.
    pub fn decode_header(
        data: &[u8],
    ) -> Result<Option<(Status, u32, usize)>, ProtocolError> {
        let magic_pos = match data
            .windows(Self::MAGIC.len())
            .position(|w| w == Self::MAGIC)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        let after_magic = magic_pos + Self::MAGIC.len();
        // status digit + space + at least one length digit + '\n' = ≥4 bytes
        if data.len() < after_magic + 4 {
            return Ok(None);
        }
        let status = match data[after_magic] {
            b'0' => Status::Ok,
            b'1' => Status::Err,
            b'2' => Status::Blocked,
            _ => return Err(ProtocolError::InvalidResponse),
        };
        if data[after_magic + 1] != b' ' {
            return Err(ProtocolError::InvalidResponse);
        }
        let len_start = after_magic + 2;
        // The LinBPQ TELNET driver often rewrites LF into CR on both
        // directions; accept either as the header terminator.
        let nl_offset = match data[len_start..].iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(o) => o,
            None => return Ok(None),
        };
        let len_str = std::str::from_utf8(&data[len_start..len_start + nl_offset])
            .map_err(|_| ProtocolError::InvalidResponse)?;
        let payload_len: u32 = len_str
            .parse()
            .map_err(|_| ProtocolError::InvalidResponse)?;
        let header_end = len_start + nl_offset + 1;
        Ok(Some((status, payload_len, header_end)))
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
    fn test_response_encode_decode() {
        let resp = Response {
            status: Status::Ok,
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

        let (status, b64_len, header_end) =
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
            payload: b"nope".to_vec(),
        };
        let mut with_prefix = b"junk banner text\r".to_vec();
        with_prefix.extend_from_slice(&resp.encode());

        let (status, b64_len, header_end) =
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
            payload: payload.clone(),
        }
        .encode();

        for &b in &encoded {
            assert!(b != 0x00 && b != b'\r' && b != 0xff);
        }

        let (_, b64_len, header_end) =
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
        assert!(Status::try_from(0x03).is_err());
    }
}

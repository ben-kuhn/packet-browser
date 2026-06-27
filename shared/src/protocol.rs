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

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let payload_len = self.payload.len() as u32;
        let mut data = Vec::with_capacity(5 + self.payload.len());
        data.push(self.status as u8);
        data.extend_from_slice(&payload_len.to_be_bytes());
        data.extend_from_slice(&self.payload);
        data
    }

    pub fn decode_header(data: &[u8]) -> Result<(Status, u32), ProtocolError> {
        if data.len() < 5 {
            return Err(ProtocolError::InvalidResponse);
        }

        let status = Status::try_from(data[0])?;
        let payload_len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);

        Ok((status, payload_len))
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
        let (status, len) = Response::decode_header(&encoded).unwrap();
        assert_eq!(status, Status::Ok);
        assert_eq!(len, 12);
    }

    #[test]
    fn test_status_codes() {
        assert_eq!(Status::try_from(0x00).unwrap(), Status::Ok);
        assert_eq!(Status::try_from(0x01).unwrap(), Status::Err);
        assert_eq!(Status::try_from(0x02).unwrap(), Status::Blocked);
        assert!(Status::try_from(0x03).is_err());
    }
}

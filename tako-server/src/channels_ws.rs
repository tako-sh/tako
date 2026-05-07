use crate::channels::{ChannelError, ChannelHeaderValue, ChannelPublishPayload};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use openssl::sha::sha1;
use pingora_http::{RequestHeader, ResponseHeader};

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_WEBSOCKET_FRAME_PAYLOAD_BYTES: u64 = crate::proxy::MAX_REQUEST_BODY_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSocketFrame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct WebSocketFrameReader {
    buffer: Vec<u8>,
}

impl WebSocketFrameReader {
    pub(crate) fn extend(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<WebSocketFrame>, ChannelError> {
        if self.buffer.len() < 2 {
            return Ok(None);
        }

        let first = self.buffer[0];
        let second = self.buffer[1];
        let fin = (first & 0x80) != 0;
        let opcode = first & 0x0f;
        if !fin {
            return Err(ChannelError::BadRequest(
                "fragmented websocket frames are not supported".to_string(),
            ));
        }

        let masked = (second & 0x80) != 0;
        let mut payload_len = u64::from(second & 0x7f);
        let mut offset = 2usize;

        if payload_len == 126 {
            if self.buffer.len() < offset + 2 {
                return Ok(None);
            }
            payload_len = u64::from(u16::from_be_bytes([
                self.buffer[offset],
                self.buffer[offset + 1],
            ]));
            offset += 2;
        } else if payload_len == 127 {
            if self.buffer.len() < offset + 8 {
                return Ok(None);
            }
            payload_len = u64::from_be_bytes([
                self.buffer[offset],
                self.buffer[offset + 1],
                self.buffer[offset + 2],
                self.buffer[offset + 3],
                self.buffer[offset + 4],
                self.buffer[offset + 5],
                self.buffer[offset + 6],
                self.buffer[offset + 7],
            ]);
            offset += 8;
        }

        if matches!(opcode, 0x8..=0xa) && payload_len > 125 {
            return Err(ChannelError::BadRequest(
                "invalid websocket control frame".to_string(),
            ));
        }
        if payload_len > MAX_WEBSOCKET_FRAME_PAYLOAD_BYTES {
            return Err(ChannelError::BadRequest(
                "websocket frame too large".to_string(),
            ));
        }

        let mask = if masked {
            if self.buffer.len() < offset + 4 {
                return Ok(None);
            }
            let mask = [
                self.buffer[offset],
                self.buffer[offset + 1],
                self.buffer[offset + 2],
                self.buffer[offset + 3],
            ];
            offset += 4;
            Some(mask)
        } else {
            None
        };

        let payload_len = usize::try_from(payload_len)
            .map_err(|_| ChannelError::BadRequest("websocket frame too large".to_string()))?;
        let total_len = offset
            .checked_add(payload_len)
            .ok_or_else(|| ChannelError::BadRequest("websocket frame too large".to_string()))?;
        if self.buffer.len() < total_len {
            return Ok(None);
        }

        let mut payload = self.buffer[offset..total_len].to_vec();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }

        self.buffer.drain(..total_len);
        Ok(Some(WebSocketFrame { opcode, payload }))
    }
}

pub(crate) fn build_websocket_upgrade_response(
    request: &RequestHeader,
) -> Result<ResponseHeader, ChannelError> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ChannelError::BadRequest("missing Sec-WebSocket-Key".to_string()))?;

    let version = request
        .headers
        .get("sec-websocket-version")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if version != "13" {
        return Err(ChannelError::BadRequest(
            "unsupported Sec-WebSocket-Version".to_string(),
        ));
    }

    let mut response = ResponseHeader::build(101, None)
        .map_err(|e| ChannelError::Storage(format!("build websocket response: {e}")))?;
    response
        .insert_header("Upgrade", "websocket")
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    response
        .insert_header("Connection", "Upgrade")
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    response
        .insert_header("Sec-WebSocket-Accept", websocket_accept(key))
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    response
        .insert_header("Cache-Control", "no-store")
        .map_err(|e| ChannelError::Storage(e.to_string()))?;
    Ok(response)
}

pub(crate) fn websocket_text_frame(text: &str) -> Vec<u8> {
    websocket_frame(0x1, text.as_bytes())
}

pub(crate) fn websocket_ping_frame(payload: &[u8]) -> Vec<u8> {
    websocket_frame(0x9, payload)
}

pub(crate) fn websocket_pong_frame(payload: &[u8]) -> Vec<u8> {
    websocket_frame(0xA, payload)
}

pub(crate) fn websocket_close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    websocket_frame(0x8, &payload)
}

pub(crate) fn parse_publish_payload(payload: &[u8]) -> Result<ChannelPublishPayload, ChannelError> {
    serde_json::from_slice(payload)
        .map_err(|_| ChannelError::BadRequest("invalid websocket publish payload".to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FirstFrameAuth {
    pub header_value: Option<ChannelHeaderValue>,
    pub last_message_id: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
#[error("malformed tako.auth envelope")]
pub(crate) struct FirstFrameError;

pub(crate) fn parse_first_frame(json: &str) -> Result<FirstFrameAuth, FirstFrameError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|_| FirstFrameError)?;
    if value.get("type").and_then(|value| value.as_str()) != Some("tako.auth") {
        return Err(FirstFrameError);
    }

    let header_value = value
        .get("token")
        .and_then(|value| value.as_str())
        .map(ChannelHeaderValue::parse);
    let last_message_id = value.get("lastMessageId").and_then(|value| match value {
        serde_json::Value::String(value) => value.parse::<i64>().ok(),
        serde_json::Value::Number(value) => value.as_i64(),
        _ => None,
    });

    Ok(FirstFrameAuth {
        header_value,
        last_message_id,
    })
}

fn websocket_accept(key: &str) -> String {
    let mut input = String::with_capacity(key.len() + WEBSOCKET_GUID.len());
    input.push_str(key);
    input.push_str(WEBSOCKET_GUID);
    BASE64_STANDARD.encode(sha1(input.as_bytes()))
}

fn websocket_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(2 + payload.len() + 8);
    frame.push(0x80 | (opcode & 0x0f));
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_upgrade_response_sets_accept_header() {
        let mut request =
            RequestHeader::build("GET", b"/channels/chat%3Aroom-123", None).expect("build request");
        request
            .insert_header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .unwrap();
        request
            .insert_header("Sec-WebSocket-Version", "13")
            .unwrap();

        let response = build_websocket_upgrade_response(&request).unwrap();
        assert_eq!(response.status.as_u16(), 101);
        assert_eq!(
            response
                .headers
                .get("sec-websocket-accept")
                .and_then(|value| value.to_str().ok()),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
    }

    #[test]
    fn websocket_reader_decodes_masked_text_frame() {
        let mut reader = WebSocketFrameReader::default();
        reader.extend(&[
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ]);

        let frame = reader.next_frame().unwrap().unwrap();
        assert_eq!(frame.opcode, 0x1);
        assert_eq!(frame.payload, b"Hello".to_vec());
    }

    #[test]
    fn websocket_reader_rejects_frames_above_proxy_body_limit() {
        let mut reader = WebSocketFrameReader::default();
        let too_large = MAX_WEBSOCKET_FRAME_PAYLOAD_BYTES + 1;
        let mut frame = vec![0x81, 0xff];
        frame.extend_from_slice(&too_large.to_be_bytes());
        frame.extend_from_slice(&[0, 0, 0, 0]);
        reader.extend(&frame);

        let err = reader.next_frame().unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn websocket_text_frame_encodes_unmasked_payload() {
        let frame = websocket_text_frame("hi");
        assert_eq!(frame, vec![0x81, 0x02, b'h', b'i']);
    }

    #[test]
    fn first_frame_auth_extracts_token_into_header_value() {
        let parsed = parse_first_frame(r#"{"type":"tako.auth","token":"Bearer abc"}"#).unwrap();
        assert_eq!(
            parsed.header_value,
            Some(ChannelHeaderValue {
                scheme: Some("Bearer".into()),
                value: "abc".into()
            })
        );
        assert_eq!(parsed.last_message_id, None);
    }

    #[test]
    fn first_frame_auth_accepts_numeric_or_string_last_message_id() {
        let parsed =
            parse_first_frame(r#"{"type":"tako.auth","token":"plain","lastMessageId":"42"}"#)
                .unwrap();
        assert_eq!(parsed.last_message_id, Some(42));

        let parsed = parse_first_frame(r#"{"type":"tako.auth","lastMessageId":43}"#).unwrap();
        assert_eq!(parsed.last_message_id, Some(43));
    }

    #[test]
    fn first_frame_auth_rejects_malformed_envelope() {
        assert!(parse_first_frame("not json").is_err());
        assert!(parse_first_frame(r#"{"type":"chat.send"}"#).is_err());
    }
}

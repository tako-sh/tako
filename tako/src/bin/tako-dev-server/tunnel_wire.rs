//! Tunnel wire format. Keep identical in tako-sh/tunnel and tako-sh/tako.
//! Each WebSocket binary message contains one named MessagePack value.
use serde::{Deserialize, Serialize};

pub const TUNNEL_PROTOCOL: &str = "tako-tunnel-v2";
pub const MAX_IN_FLIGHT: usize = 32;
pub const CHUNK_SIZE: usize = 64 * 1024;
// A byte budget, with a minimum charge to bound tiny-frame metadata/queue costs.
pub const RESPONSE_WINDOW: usize = 128 * 1024;
pub const MIN_CHUNK_CREDIT: usize = 1024;

pub fn response_chunk_cost(len: usize) -> usize {
    len.max(MIN_CHUNK_CREDIT)
}
pub const MAX_WS_FRAME: usize = 1024 * 1024;
pub const MAX_WIRE_MESSAGE: usize = 2 * 1024 * 1024 + CHUNK_SIZE;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Response(TunnelResponseMessage),
    ResponseChunk(TunnelBodyChunk),
    ResponseEnd { id: String },
    ResponseError { id: String },
    WebSocketResponse(TunnelWebSocketResponseMessage),
    WebSocketFrame(TunnelWebSocketFrameMessage),
    WebSocketClose(TunnelWebSocketCloseMessage),
    Ping { nonce: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Request(TunnelRequestMessage),
    Cancel { id: String },
    ResponseCredit { id: String, bytes: usize },
    WebSocketOpen(TunnelWebSocketOpenMessage),
    WebSocketFrame(TunnelWebSocketFrameMessage),
    WebSocketClose(TunnelWebSocketCloseMessage),
    Pong { nonce: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelRequestMessage {
    pub id: String,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelResponseMessage {
    pub id: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelBodyChunk {
    pub id: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelWebSocketOpenMessage {
    pub id: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelWebSocketResponseMessage {
    pub id: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelWebSocketFrameMessage {
    pub id: String,
    pub kind: TunnelWebSocketFrameKind,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TunnelWebSocketCloseMessage {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelWebSocketFrameKind {
    Text,
    Binary,
    Ping,
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_payload_is_binary_and_roundtrips_without_expansion() {
        let data: Vec<u8> = (0..=255).cycle().take(CHUNK_SIZE).collect();
        let message = ClientMessage::ResponseChunk(TunnelBodyChunk {
            id: "request".into(),
            data: data.clone(),
        });
        let encoded = rmp_serde::to_vec_named(&message).unwrap();
        assert!(encoded.len() < data.len() + 100);
        let ClientMessage::ResponseChunk(decoded) = rmp_serde::from_slice(&encoded).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(decoded.data, data);
    }

    // The same fixture is checked by both independently built protocol peers.
    #[test]
    fn credit_wire_fixture() {
        let wire = b"\x83\xa4type\xafresponse_credit\xa2id\xa1r\xa5bytes\xcd\x04\x00";
        assert!(
            matches!(rmp_serde::from_slice::<ServerMessage>(wire).unwrap(), ServerMessage::ResponseCredit { id, bytes: 1024 } if id == "r")
        );
        assert_eq!(
            rmp_serde::to_vec_named(&ServerMessage::ResponseCredit {
                id: "r".into(),
                bytes: 1024
            })
            .unwrap(),
            wire
        );
    }
}

//! Pulse control protocol, version 2 (described in `docs/protocol.md`).
//!
//! Transport: Unix domain stream socket at `$XDG_RUNTIME_DIR/pulse-macro/engine.sock`.
//! Framing: 4-byte big-endian length followed by a UTF-8 JSON document,
//! at most [`MAX_FRAME`] bytes. Clients send [`Request`]s; the engine answers
//! each with a [`ServerMessage::Response`] carrying the same `request_id`, and
//! pushes low-rate [`ServerMessage::Event`]s to subscribed clients. The
//! protocol carries control transitions only — never per-click traffic.

pub mod client;
mod frame;
mod messages;

pub use client::{Client, ClientError};
pub use frame::{FrameDecoder, MAX_FRAME, ProtocolError, encode_frame};
pub use messages::*;

pub const PROTOCOL_VERSION: u32 = 2;

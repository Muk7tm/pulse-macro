//! Length-delimited framing.

use serde::Serialize;

/// Maximum frame body size (256 KiB). Pulse's messages stay well below this.
pub const MAX_FRAME: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME}-byte limit")]
    FrameTooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("cannot encode message: {0}")]
    Encode(String),
}

pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(msg).map_err(|e| ProtocolError::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(ProtocolError::FrameTooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Incremental decoder. Rejects oversized frames from the header alone, so a
/// peer cannot make the reader buffer more than one maximal frame.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Next complete frame body, if any.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(ProtocolError::FrameTooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_reads() {
        let f = encode_frame(&serde_json::json!({"a": 1})).expect("encode");
        let mut d = FrameDecoder::new();
        d.push(&f[..3]);
        assert!(d.next_frame().expect("ok").is_none());
        d.push(&f[3..]);
        d.push(&f);
        assert_eq!(d.next_frame().expect("ok").as_deref(), Some(&b"{\"a\":1}"[..]));
        assert_eq!(d.next_frame().expect("ok").as_deref(), Some(&b"{\"a\":1}"[..]));
        assert!(d.next_frame().expect("ok").is_none());
    }

    #[test]
    fn oversized_header_rejected_before_buffering() {
        let mut d = FrameDecoder::new();
        d.push(&((MAX_FRAME as u32) + 1).to_be_bytes());
        assert!(matches!(d.next_frame(), Err(ProtocolError::FrameTooLarge(_))));
    }
}

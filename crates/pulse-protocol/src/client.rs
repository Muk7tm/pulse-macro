//! Blocking client used by `pulse-cli` and `pulse-ui`.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::frame::{FrameDecoder, ProtocolError, encode_frame};
use crate::messages::{Command, EngineEvent, ErrorBody, Request, Response, ResponseData, ServerMessage};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the Pulse engine is not running (no socket at {0})")]
    NotRunning(PathBuf),
    #[error("connection error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("{}", .0.message)]
    Engine(ErrorBody),
    #[error("timed out waiting for the engine")]
    Timeout,
    #[error("the engine closed the connection")]
    Closed,
    #[error(
        "an older Pulse engine is running (protocol {engine}; this tool needs {}). Restart Pulse: use \"Restart Pulse\" in the window, or stop the old engine and start the new one",
        crate::PROTOCOL_VERSION
    )]
    VersionMismatch { engine: u64 },
}

/// A frame that does not parse may come from an engine speaking another
/// protocol version; report that instead of a generic parse error.
fn version_mismatch(body: &[u8]) -> Option<ClientError> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let engine = v.get("protocol_version")?.as_u64()?;
    (engine != u64::from(crate::PROTOCOL_VERSION)).then_some(ClientError::VersionMismatch { engine })
}

pub struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    next_id: u64,
    events: VecDeque<EngineEvent>,
    timeout: Duration,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::NotRunning(path.to_owned()),
            _ => ClientError::Io(e),
        })?;
        Ok(Client {
            stream,
            decoder: FrameDecoder::new(),
            next_id: 1,
            events: VecDeque::new(),
            timeout: Duration::from_secs(10),
        })
    }

    /// Per-request response timeout (backend switches may take ~2 s).
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    fn read_message(&mut self, deadline: Option<Instant>) -> Result<Option<ServerMessage>, ClientError> {
        loop {
            if let Some(body) = self.decoder.next_frame()? {
                return match serde_json::from_slice::<ServerMessage>(&body) {
                    Ok(msg) => Ok(Some(msg)),
                    Err(e) => {
                        Err(version_mismatch(&body).unwrap_or_else(|| ProtocolError::Malformed(e.to_string()).into()))
                    }
                };
            }
            let remaining = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Ok(None);
                    }
                    Some(d - now)
                }
                None => None,
            };
            self.stream.set_read_timeout(remaining)?;
            let mut buf = [0u8; 16 * 1024];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(ClientError::Closed),
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Send a command and wait for its response. Events received meanwhile
    /// are queued for [`Client::next_event`].
    pub fn request(&mut self, command: Command) -> Result<Response, ClientError> {
        let id = format!("c{}", self.next_id);
        self.next_id += 1;
        let frame = encode_frame(&Request::new(id.clone(), command))?;
        self.stream.write_all(&frame)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.read_message(Some(deadline))? {
                None => return Err(ClientError::Timeout),
                Some(ServerMessage::Event(e)) => {
                    if self.events.len() < 256 {
                        self.events.push_back(e.event);
                    }
                }
                Some(ServerMessage::Response(r)) if r.request_id == id => return Ok(r),
                Some(ServerMessage::Response(_)) => {}
            }
        }
    }

    /// Like `request`, but turns `ok: false` into `ClientError::Engine`.
    pub fn call(&mut self, command: Command) -> Result<Response, ClientError> {
        let r = self.request(command)?;
        if r.ok {
            Ok(r)
        } else {
            Err(ClientError::Engine(r.error.unwrap_or(ErrorBody {
                code: crate::ErrorCode::Internal,
                message: "engine reported failure without details".into(),
                issues: Vec::new(),
            })))
        }
    }

    pub fn call_data(&mut self, command: Command) -> Result<Option<ResponseData>, ClientError> {
        Ok(self.call(command)?.data)
    }

    /// Wait for the next pushed event (after `Command::Subscribe`).
    pub fn next_event(&mut self, timeout: Option<Duration>) -> Result<Option<EngineEvent>, ClientError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            match self.read_message(deadline)? {
                None => return Ok(None),
                Some(ServerMessage::Event(e)) => return Ok(Some(e.event)),
                Some(ServerMessage::Response(_)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_engine_frames_are_reported_as_version_mismatch() {
        // What a protocol-1 engine sends back: a state this build no longer knows.
        let v1 = br#"{"type":"response","protocol_version":1,"request_id":"c1","ok":false,"state":"armed","active_backend":"xtest","error":{"code":"unsupported_version","message":"protocol version 2 is not supported"}}"#;
        assert!(serde_json::from_slice::<ServerMessage>(v1).is_err());
        let err = version_mismatch(v1).expect("detected");
        assert!(matches!(err, ClientError::VersionMismatch { engine: 1 }));
        assert!(err.to_string().contains("Restart Pulse"));
        assert!(version_mismatch(b"not json").is_none());
        let current = format!(r#"{{"protocol_version":{}}}"#, crate::PROTOCOL_VERSION);
        assert!(version_mismatch(current.as_bytes()).is_none());
    }
}

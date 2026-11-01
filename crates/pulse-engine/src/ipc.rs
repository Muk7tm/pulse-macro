//! Unix-socket control server and single-instance lock.
//!
//! - Socket directory `$XDG_RUNTIME_DIR/pulse-macro` is created `0700` and must be
//!   owned by the engine's user; the socket itself is `0600`.
//! - A `flock` on `engine.lock` guarantees a single engine. Only while that
//!   lock is held is an existing socket path considered stale, and it is
//!   unlinked only if it is a socket owned by this user.
//! - Every accepted peer is authenticated with `SO_PEERCRED`; other UIDs are
//!   disconnected without reading a byte.
//! - Frames are bounded; per-client output is bounded (slow clients are
//!   dropped rather than buffering without limit).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use pulse_protocol::{FrameDecoder, ProtocolError, Request, ServerMessage, encode_frame};
use rustix::fs::{FlockOperation, flock};

pub const MAX_CLIENTS: usize = 16;
const MAX_OUTBUF: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("another Pulse engine is already running (lock {0} is held)")]
    AlreadyRunning(PathBuf),
    #[error("{0}")]
    Insecure(String),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path, source: io::Error) -> IpcError {
    IpcError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Create or validate the private runtime directory.
pub fn prepare_runtime_dir(dir: &Path) -> Result<(), IpcError> {
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
                fs::create_dir(dir).map_err(|e| io_err(dir, e))?;
            } else {
                return Err(io_err(dir, e));
            }
        }
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| io_err(dir, e))?;
    if !meta.is_dir() {
        return Err(IpcError::Insecure(format!("{} is not a directory", dir.display())));
    }
    let uid = rustix::process::geteuid().as_raw();
    if meta.uid() != uid {
        return Err(IpcError::Insecure(format!(
            "{} is owned by uid {}, not {uid}",
            dir.display(),
            meta.uid()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| io_err(dir, e))?;
    }
    Ok(())
}

pub struct InstanceLock {
    _file: File,
    pub path: PathBuf,
}

/// Acquire the single-instance lock (released automatically on exit).
pub fn acquire_lock(path: &Path) -> Result<InstanceLock, IpcError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| io_err(path, e))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Err(IpcError::AlreadyRunning(path.to_owned())),
        Err(e) => return Err(io_err(path, e.into())),
    }
    let mut f = &file;
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    Ok(InstanceLock {
        _file: file,
        path: path.to_owned(),
    })
}

struct Conn {
    stream: UnixStream,
    decoder: FrameDecoder,
    out: Vec<u8>,
    subscribed: bool,
    closing: bool,
}

pub enum ClientInput {
    Request(Request),
    /// Unparseable request; reply with an error then keep going.
    Malformed(String),
}

pub struct IpcServer {
    listener: UnixListener,
    path: PathBuf,
    clients: Vec<Option<Conn>>,
    pub rejected_peers: u64,
}

impl IpcServer {
    /// Bind the socket. The instance lock must already be held.
    pub fn bind(path: &Path, _lock: &InstanceLock) -> Result<IpcServer, IpcError> {
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                let uid = rustix::process::geteuid().as_raw();
                if !meta.file_type().is_socket() || meta.uid() != uid {
                    return Err(IpcError::Insecure(format!(
                        "{} exists and is not a socket owned by this user; refusing to remove it",
                        path.display()
                    )));
                }
                // We hold the instance lock, so no live engine owns it.
                fs::remove_file(path).map_err(|e| io_err(path, e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(path, e)),
        }
        let listener = UnixListener::bind(path).map_err(|e| io_err(path, e))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| io_err(path, e))?;
        listener.set_nonblocking(true).map_err(|e| io_err(path, e))?;
        Ok(IpcServer {
            listener,
            path: path.to_owned(),
            clients: (0..MAX_CLIENTS).map(|_| None).collect(),
            rejected_peers: 0,
        })
    }

    pub fn listener_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }

    pub fn client_fd(&self, slot: usize) -> Option<BorrowedFd<'_>> {
        self.clients.get(slot)?.as_ref().map(|c| c.stream.as_fd())
    }

    pub fn has_pending_output(&self, slot: usize) -> bool {
        self.clients
            .get(slot)
            .and_then(|c| c.as_ref())
            .is_some_and(|c| !c.out.is_empty())
    }

    /// Accept pending connections. Returns the slots of new authorized clients.
    pub fn accept(&mut self) -> Vec<usize> {
        let mut new = Vec::new();
        loop {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let uid = rustix::process::geteuid().as_raw();
            match rustix::net::sockopt::socket_peercred(&stream) {
                Ok(cred) if cred.uid.as_raw() == uid => {}
                _ => {
                    self.rejected_peers += 1;
                    continue; // dropped: connection closed unread
                }
            }
            let Some(slot) = self.clients.iter().position(Option::is_none) else {
                continue; // at capacity: refuse
            };
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            self.clients[slot] = Some(Conn {
                stream,
                decoder: FrameDecoder::new(),
                out: Vec::new(),
                subscribed: false,
                closing: false,
            });
            new.push(slot);
        }
        new
    }

    /// Read everything available from a client. Returns parsed inputs and
    /// whether the client is still connected.
    pub fn read_client(&mut self, slot: usize) -> (Vec<ClientInput>, bool) {
        let mut inputs = Vec::new();
        let Some(conn) = self.clients.get_mut(slot).and_then(|c| c.as_mut()) else {
            return (inputs, false);
        };
        let mut buf = [0u8; 16 * 1024];
        let mut alive = true;
        loop {
            match conn.stream.read(&mut buf) {
                Ok(0) => {
                    alive = false;
                    break;
                }
                Ok(n) => conn.decoder.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    alive = false;
                    break;
                }
            }
            if conn.decoder.buffered() > pulse_protocol::MAX_FRAME + 4 {
                break;
            }
        }
        loop {
            match conn.decoder.next_frame() {
                Ok(Some(body)) => match serde_json::from_slice::<Request>(&body) {
                    Ok(r) => inputs.push(ClientInput::Request(r)),
                    Err(e) => inputs.push(ClientInput::Malformed(e.to_string())),
                },
                Ok(None) => break,
                Err(ProtocolError::FrameTooLarge(n)) => {
                    // Cannot resynchronize the stream: report and disconnect.
                    inputs.push(ClientInput::Malformed(format!("frame of {n} bytes exceeds the limit")));
                    conn.closing = true;
                    break;
                }
                Err(e) => {
                    inputs.push(ClientInput::Malformed(e.to_string()));
                    conn.closing = true;
                    break;
                }
            }
        }
        if !alive {
            self.clients[slot] = None;
        }
        (inputs, alive)
    }

    pub fn set_subscribed(&mut self, slot: usize) {
        if let Some(c) = self.clients.get_mut(slot).and_then(|c| c.as_mut()) {
            c.subscribed = true;
        }
    }

    /// Queue a message and try to write it. Returns false if the client was
    /// dropped.
    pub fn send(&mut self, slot: usize, msg: &ServerMessage) -> bool {
        let Ok(frame) = encode_frame(msg) else { return true };
        let Some(conn) = self.clients.get_mut(slot).and_then(|c| c.as_mut()) else {
            return false;
        };
        if conn.out.len() + frame.len() > MAX_OUTBUF {
            self.clients[slot] = None;
            return false;
        }
        conn.out.extend_from_slice(&frame);
        self.flush(slot)
    }

    pub fn broadcast(&mut self, msg: &ServerMessage) {
        for slot in 0..self.clients.len() {
            if self.clients[slot].as_ref().is_some_and(|c| c.subscribed) {
                self.send(slot, msg);
            }
        }
    }

    /// Write pending output. Returns false if the client was dropped.
    pub fn flush(&mut self, slot: usize) -> bool {
        let Some(conn) = self.clients.get_mut(slot).and_then(|c| c.as_mut()) else {
            return false;
        };
        while !conn.out.is_empty() {
            match conn.stream.write(&conn.out) {
                Ok(0) => {
                    self.clients[slot] = None;
                    return false;
                }
                Ok(n) => {
                    conn.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.clients[slot] = None;
                    return false;
                }
            }
        }
        if conn.closing {
            self.clients[slot] = None;
            return false;
        }
        true
    }

    pub fn disconnect(&mut self, slot: usize) {
        if let Some(c) = self.clients.get_mut(slot) {
            *c = None;
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.iter().filter(|c| c.is_some()).count()
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

use std::{
    io,
    io::{
        Read,
        Write,
    },
    os::unix::{
        io::AsRawFd,
        net::UnixStream,
    },
    path::Path,
    sync::atomic::{
        AtomicBool,
        Ordering,
    },
    thread,
};

use anyhow::{
    anyhow,
    Context,
};
use byteorder::{
    LittleEndian,
    ReadBytesExt,
    WriteBytesExt,
};
use serde_derive::{
    Deserialize,
    Serialize,
};
use tracing::{
    debug,
    info,
    trace,
};

use super::{
    consts,
    tty,
};

/// ConnectHeader is the blob of metadata that a client transmits when it
/// first connections. It uses an enum to allow different connection types
/// to be initiated on the same socket. The ConnectHeader is always prefixed
/// with a 4 byte little endian unsigned word to indicate length.
#[derive(Serialize, Deserialize, Debug)]
pub enum ConnectHeader {
    /// Attach to the named session indicated by the given header.
    ///
    /// Responds with an AttachReplyHeader.
    Attach(AttachHeader),
    /// List all of the currently active sessions.
    List,
    /// A message for a named, running sessions. This
    /// provides a mechanism for RPC-like calls to be
    /// made to running sessions. Messages are only
    /// delivered if there is currently a client attached
    /// to the session.
    SessionMessage(SessionMessageRequest),
    /// A message to request that a list of running
    /// sessions get detached from.
    Detach(DetachRequest),
    /// A message to request that a list of running
    /// sessions get killed.
    Kill(KillRequest),
}

/// KillRequest represents a request to kill
/// from the given named sessions.
#[derive(Serialize, Deserialize, Debug)]
pub struct KillRequest {
    /// The sessions to detach
    pub sessions: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct KillReply {
    pub not_found_sessions: Vec<String>,
}

/// DetachRequest represents a request to detach
/// from the given named sessions.
#[derive(Serialize, Deserialize, Debug)]
pub struct DetachRequest {
    /// The sessions to detach
    pub sessions: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DetachReply {
    pub not_found_sessions: Vec<String>,
    pub not_attached_sessions: Vec<String>,
}

/// SessionMessageRequest represents a request that
/// ought to be routed to the session indicated by
/// `session_name`.
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionMessageRequest {
    /// The session to route this request to.
    pub session_name: String,
    /// The actual message to send to the session.
    pub payload: SessionMessageRequestPayload,
}

/// SessionMessageRequestPayload contains a request for
/// a running session.
#[derive(Serialize, Deserialize, Debug)]
pub enum SessionMessageRequestPayload {
    /// Resize a named session's pty. Generated when
    /// a `shpool attach` process receives a SIGWINCH.
    Resize(ResizeRequest),
    /// Detach the given session. Generated internally
    /// by the server from a batch detach request.
    Detach,
}

/// ResizeRequest resizes the pty for a given named session.
/// We use an out-of-band request rather than doing this
/// in the input stream because we don't want to have to
/// introduce a framing protocol for the input stream.
#[derive(Serialize, Deserialize, Debug)]
pub struct ResizeRequest {
    /// The size of the client's tty
    pub tty_size: tty::Size,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum SessionMessageReply {
    /// The session was not found in the session table
    NotFound,
    /// There is not terminal attached to the session so
    /// it can't handle messages right now.
    NotAttached,
    /// The response to a resize message
    Resize(ResizeReply),
    /// The response to a detach message
    Detach(SessionMessageDetachReply),
}

/// A reply to a detach message
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum SessionMessageDetachReply {
    Ok,
}

/// A reply to a resize message
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum ResizeReply {
    Ok,
}

/// AttachHeader is the blob of metadata that a client transmits when it
/// first dials into the shpool indicating which shell it wants to attach
/// to.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct AttachHeader {
    /// The name of the session to create or attach to.
    pub name: String,
    /// The size of the local tty. Passed along so that the remote
    /// pty can be kept in sync (important so curses applications look
    /// right).
    pub local_tty_size: tty::Size,
    /// A subset of the environment of the shell that `shpool attach` is run
    /// in. Contains only some variables needed to set up the shell when
    /// shpool forks off a process. For now the list is just `SSH_AUTH_SOCK`
    /// and `TERM`.
    pub local_env: Vec<(String, String)>,
}

impl AttachHeader {
    pub fn local_env_get<'a>(&'a self, var: &str) -> Option<&'a str> {
        for (key, val) in self.local_env.iter() {
            if var == key {
                return Some(val.as_str());
            }
        }

        None
    }
}

/// AttachReplyHeader is the blob of metadata that the shpool service prefixes
/// the data stream with after an attach. In can be used to indicate a connection
/// error.
#[derive(Serialize, Deserialize, Debug)]
pub struct AttachReplyHeader {
    pub status: AttachStatus,
}

/// ListReply is contains a list of active sessions to be displayed to the user.
#[derive(Serialize, Deserialize)]
pub struct ListReply {
    pub sessions: Vec<Session>,
}

/// Session describes an active session.
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub started_at_unix_ms: i64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LocalCommandSetMetadataReply {
    pub status: LocalCommandSetMetadataStatus,
}
#[derive(Serialize, Deserialize, Debug)]
pub enum LocalCommandSetMetadataStatus {
    /// Indicates we timed out waiting to link up with the remote command
    /// thread.
    Timeout,
    /// We successfully released the lock and allowed the attach
    /// process to proceed.
    Ok,
}

/// AttachStatus indicates what happened during an attach attempt.
#[derive(PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum AttachStatus {
    /// Attached indicates that there was an existing shell session with
    /// the given name, and `shpool attach` successfully connected to it.
    Attached,
    /// Created indicates that there was no existing shell session with the
    /// given name, so `shpool` created a new one.
    Created,
    /// Busy indicates that there is an existing shell session with the given
    /// name, but another `shpool attach` session is currently connected to
    /// it, so the connection attempt was rejected.
    Busy,
    /// Forbidden indicates that the daemon has rejected the connection
    /// attempt for security reasons.
    Forbidden(String),
    /// Some unexpected error
    UnexpectedError(String),
}

/// FrameKind is a tag that indicates what type of frame is being transmitted
/// through the socket.
#[derive(Copy, Clone, Debug)]
pub enum ChunkKind {
    Data = 0,
    Heartbeat = 1,
}

impl ChunkKind {
    fn from_u8(v: u8) -> anyhow::Result<Self> {
        match v {
            0 => Ok(ChunkKind::Data),
            1 => Ok(ChunkKind::Heartbeat),
            _ => Err(anyhow!("unknown FrameKind {}", v)),
        }
    }
}

/// Chunk represents of a chunk of data in the output stream
///
/// format:
///
/// ```
/// 1 byte: kind tag
/// little endian 4 byte word: length prefix
/// N bytes: data
/// ```
#[derive(Debug)]
pub struct Chunk<'data> {
    pub kind: ChunkKind,
    pub buf: &'data [u8],
}

impl<'data> Chunk<'data> {
    pub fn write_to<W>(&self, w: &mut W, stop: &AtomicBool) -> io::Result<()>
    where
        W: std::io::Write,
    {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            if let Err(e) = w.write_u8(self.kind as u8) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    trace!("chunk: writing tag: WouldBlock");
                    thread::sleep(consts::PIPE_POLL_DURATION);
                    continue;
                }
                return Err(e);
            }
            break;
        }

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            if let Err(e) = w.write_u32::<LittleEndian>(self.buf.len() as u32) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    trace!("chunk: writing length prefix: WouldBlock");
                    thread::sleep(consts::PIPE_POLL_DURATION);
                    continue;
                }
                return Err(e);
            }
            break;
        }

        let mut to_write = &self.buf[..];
        while to_write.len() > 0 {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            let nwritten = match w.write(&to_write) {
                Ok(n) => n,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("chunk: writing buffer: WouldBlock");
                        thread::sleep(consts::PIPE_POLL_DURATION);
                        continue;
                    }
                    return Err(e);
                },
            };
            to_write = &to_write[nwritten..];
        }

        Ok(())
    }

    pub fn read_into<R>(r: &mut R, buf: &'data mut [u8]) -> anyhow::Result<Self>
    where
        R: std::io::Read,
    {
        let kind = r.read_u8()?;
        let len = r.read_u32::<LittleEndian>()? as usize;
        if len as usize > buf.len() {
            return Err(anyhow!(
                "chunk of size {} exceeds size limit of {} bytes",
                len,
                buf.len()
            ));
        }
        r.read_exact(&mut buf[..len])?;

        Ok(Chunk {
            kind: ChunkKind::from_u8(kind)?,
            buf: &buf[..len],
        })
    }
}

pub struct Client {
    pub stream: UnixStream,
}

impl Client {
    pub fn new<P: AsRef<Path>>(sock: P) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(sock).context("connecting to shpool")?;
        Ok(Client { stream })
    }

    pub fn write_connect_header(&mut self, header: ConnectHeader) -> anyhow::Result<()> {
        let serialize_stream = self
            .stream
            .try_clone()
            .context("cloning stream for reply")?;
        bincode::serialize_into(serialize_stream, &header).context("writing reply")?;

        Ok(())
    }

    pub fn read_reply<'data, R>(&mut self) -> anyhow::Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        let reply: R = bincode::deserialize_from(&mut self.stream).context("parsing header")?;
        Ok(reply)
    }

    /// pipe_bytes suffles bytes from std{in,out} to the unix
    /// socket and back again. It is the main loop of
    /// `shpool attach`.
    pub fn pipe_bytes(self) -> anyhow::Result<()> {
        let stop = AtomicBool::new(false);

        let mut read_client_stream = self.stream.try_clone().context("cloning read stream")?;
        let mut write_client_stream = self.stream.try_clone().context("cloning read stream")?;

        thread::scope(|s| {
            // stdin -> sock
            let stdin_to_sock_h = s.spawn(|| -> anyhow::Result<()> {
                info!("pipe_bytes: stdin->sock thread spawned");

                let mut stdin = std::io::stdin().lock();
                let mut buf = vec![0; consts::BUF_SIZE];

                nix::fcntl::fcntl(
                    stdin.as_raw_fd(),
                    nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
                )
                .context("setting stdin nonblocking")?;

                loop {
                    if stop.load(Ordering::Relaxed) {
                        info!("pipe_bytes: stdin->sock: recvd stop msg (1)");
                        return Ok(());
                    }

                    let nread = match stdin.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                trace!("pipe_bytes: stdin->sock: read: WouldBlock");
                                thread::sleep(consts::PIPE_POLL_DURATION);
                                continue;
                            }
                            return Err(e).context("reading stdin from user");
                        },
                    };

                    debug!("pipe_bytes: stdin->sock: read {} bytes", nread);

                    let mut to_write = &buf[..nread];
                    debug!(
                        "pipe_bytes: stdin->sock: created to_write='{}'",
                        String::from_utf8_lossy(to_write)
                    );
                    while to_write.len() > 0 {
                        if stop.load(Ordering::Relaxed) {
                            info!("pipe_bytes: stdin->sock: recvd stop msg (2)");
                            return Ok(());
                        }

                        let nwritten = write_client_stream
                            .write(to_write)
                            .context("writing chunk to server")?;
                        to_write = &to_write[nwritten..];
                        trace!(
                            "pipe_bytes: stdin->sock: to_write={}",
                            String::from_utf8_lossy(to_write)
                        );
                    }

                    write_client_stream.flush().context("flushing client")?;
                }
            });

            // sock -> stdout
            let sock_to_stdout_h = s.spawn(|| -> anyhow::Result<()> {
                info!("pipe_bytes: sock->stdout thread spawned");

                let mut stdout = std::io::stdout().lock();
                let mut buf = vec![0; consts::BUF_SIZE];

                loop {
                    if stop.load(Ordering::Relaxed) {
                        info!("pipe_bytes: sock->stdout: recvd stop msg (1)");
                        return Ok(());
                    }

                    let chunk = Chunk::read_into(&mut read_client_stream, &mut buf)
                        .context("reading output chunk from daemon")?;

                    if chunk.buf.len() > 0 {
                        debug!(
                            "pipe_bytes: sock->stdout: chunk='{}' kind={:?} len={}",
                            String::from_utf8_lossy(chunk.buf),
                            chunk.kind,
                            chunk.buf.len()
                        );
                    }

                    let mut to_write = &chunk.buf[..];
                    match chunk.kind {
                        ChunkKind::Heartbeat => {
                            trace!("pipe_bytes: got heartbeat chunk");
                        },
                        ChunkKind::Data => {
                            while to_write.len() > 0 {
                                if stop.load(Ordering::Relaxed) {
                                    info!("pipe_bytes: sock->stdout: recvd stop msg (2)");
                                    return Ok(());
                                }

                                debug!("pipe_bytes: sock->stdout: about to select on stdout");
                                let mut stdout_set = nix::sys::select::FdSet::new();
                                stdout_set.insert(stdout.as_raw_fd());
                                let mut poll_dur = consts::PIPE_POLL_DURATION_TIMEVAL.clone();
                                let nready = nix::sys::select::select(
                                    None,
                                    None,
                                    Some(&mut stdout_set),
                                    None,
                                    Some(&mut poll_dur),
                                )
                                .context("selecting on stdout")?;
                                if nready == 0 || !stdout_set.contains(stdout.as_raw_fd()) {
                                    continue;
                                }

                                let nwritten =
                                    stdout.write(to_write).context("writing chunk to stdout")?;
                                debug!("pipe_bytes: sock->stdout: wrote {} stdout bytes", nwritten);
                                to_write = &to_write[nwritten..];
                            }

                            if let Err(e) = stdout.flush() {
                                if e.kind() == std::io::ErrorKind::WouldBlock {
                                    // If the fd is busy, we are likely just getting
                                    // flooded with output and don't need to worry about
                                    // flushing every last byte. Flushing is really
                                    // about interactive situations where we want to
                                    // see echoed bytes immediately.
                                    continue;
                                }
                            }
                            debug!("pipe_bytes: sock->stdout: flushed stdout");
                        },
                    }
                }
            });

            loop {
                if stdin_to_sock_h.is_finished() || sock_to_stdout_h.is_finished() {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                thread::sleep(consts::JOIN_POLL_DURATION);
            }
            match stdin_to_sock_h.join() {
                Ok(v) => v?,
                Err(panic_err) => std::panic::resume_unwind(panic_err),
            }
            match sock_to_stdout_h.join() {
                Ok(v) => v?,
                Err(panic_err) => std::panic::resume_unwind(panic_err),
            }

            Ok(())
        })
    }
}

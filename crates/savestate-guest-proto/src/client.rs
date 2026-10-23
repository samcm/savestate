//! The host side: one [`Client`] per VM, one vsock connection per call.

use crate::proto::{self, GuestMessage, HostMessage, guest_message, host_message};
use crate::{CHUNK, PORT, body_len, encode};
use prost::Message;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_vsock::{OwnedReadHalf, OwnedWriteHalf, VsockAddr, VsockStream};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Connecting to or talking with the agent failed: it is not up yet, the VM was
    /// restored under the connection, or the VM is gone.
    #[error("guest agent connection: {0}")]
    Io(#[from] io::Error),
    /// The agent could not do what was asked; its message.
    #[error("guest agent: {0}")]
    Guest(String),
    /// The agent answered something this client does not understand, or the request
    /// is too large to send.
    #[error("guest agent protocol: {0}")]
    Protocol(String),
}

/// How a program run by [`Client::exec`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
}

/// The guest agent of one VM. Every call opens its own connection, so calls may run
/// concurrently, and none is left half-done on the wire if its future is dropped
/// (dropping an exec leaves the program running with stdin closed).
#[derive(Clone, Debug)]
pub struct Client {
    cid: u32,
    port: u32,
}

impl Client {
    /// The agent of the VM with vsock context id `cid` (QEMU's `vhost-vsock-pci`
    /// `guest-cid`), on [`PORT`].
    pub fn vsock(cid: u32) -> Self {
        Self { cid, port: PORT }
    }

    /// The agent's version and the guest's boot id. Fails with [`Error::Io`] until the
    /// agent is up.
    pub async fn hello(&self) -> Result<proto::HelloReply, Error> {
        let (mut rd, _wr) = self
            .call(host_message::Body::Hello(proto::Hello {}))
            .await?;
        match recv(&mut rd).await? {
            guest_message::Body::Hello(h) => Ok(h),
            other => Err(unexpected(&other)),
        }
    }

    /// Runs `exec.argv` in the guest, feeding it `stdin` and copying its output to
    /// `stdout` and `stderr` until it exits. A program that fails to start is
    /// [`Error::Guest`].
    pub async fn exec(
        &self,
        exec: proto::Exec,
        mut stdin: impl AsyncRead + Unpin,
        mut stdout: impl AsyncWrite + Unpin,
        mut stderr: impl AsyncWrite + Unpin,
    ) -> Result<ExitStatus, Error> {
        let (mut rd, mut wr) = self.call(host_message::Body::Exec(exec)).await?;
        let input = async move {
            let mut buf = vec![0; CHUNK];
            loop {
                let n = stdin.read(&mut buf).await?;
                let msg = host_message::Body::Stdin(proto::Stdin {
                    data: buf[..n].to_vec(),
                    eof: n == 0,
                });
                // The guest stops reading once the program has exited; its answer
                // arrives through `output`.
                if send(&mut wr, msg).await.is_err() || n == 0 {
                    return Ok::<_, Error>(());
                }
            }
        };
        let output = async {
            loop {
                match recv(&mut rd).await? {
                    guest_message::Body::Output(o) => {
                        if o.stream() == proto::Stream::Stderr {
                            stderr.write_all(&o.data).await?;
                        } else {
                            stdout.write_all(&o.data).await?;
                        }
                    }
                    guest_message::Body::Exited(e) => {
                        stdout.flush().await?;
                        stderr.flush().await?;
                        return match e.status {
                            Some(proto::exited::Status::Code(c)) => Ok(ExitStatus::Code(c)),
                            Some(proto::exited::Status::Signal(s)) => Ok(ExitStatus::Signal(s)),
                            None => Err(Error::Protocol("exit without a status".into())),
                        };
                    }
                    other => return Err(unexpected(&other)),
                }
            }
        };
        tokio::pin!(input, output);
        let mut input_done = false;
        loop {
            tokio::select! {
                r = &mut input, if !input_done => {
                    r?;
                    input_done = true;
                }
                r = &mut output => return r,
            }
        }
    }

    /// Copies the guest file at `path` into `out`; returns its size.
    pub async fn read_file(
        &self,
        path: &str,
        mut out: impl AsyncWrite + Unpin,
    ) -> Result<u64, Error> {
        let (mut rd, _wr) = self
            .call(host_message::Body::ReadFile(proto::ReadFile {
                path: path.into(),
            }))
            .await?;
        let mut bytes = 0;
        loop {
            match recv(&mut rd).await? {
                guest_message::Body::Chunk(c) => {
                    out.write_all(&c.data).await?;
                    bytes += c.data.len() as u64;
                    if c.eof {
                        out.flush().await?;
                        return Ok(bytes);
                    }
                }
                other => return Err(unexpected(&other)),
            }
        }
    }

    /// Replaces the guest file at `path` with everything `src` yields, with permission
    /// bits `mode` (0: 0644); returns the size written. The file changes atomically,
    /// once all of it is in the guest.
    pub async fn write_file(
        &self,
        path: &str,
        mode: u32,
        mut src: impl AsyncRead + Unpin,
    ) -> Result<u64, Error> {
        let (mut rd, mut wr) = self
            .call(host_message::Body::WriteFile(proto::WriteFile {
                path: path.into(),
                mode,
            }))
            .await?;
        let mut buf = vec![0; CHUNK];
        let mut bytes = 0;
        loop {
            let n = src.read(&mut buf).await?;
            let chunk = proto::Chunk {
                data: buf[..n].to_vec(),
                eof: n == 0,
            };
            send(&mut wr, host_message::Body::Chunk(chunk)).await?;
            bytes += n as u64;
            if n == 0 {
                break;
            }
        }
        done(&mut rd).await?;
        Ok(bytes)
    }

    /// The pre-checkpoint hook: the guest syncs its filesystems and drops its page
    /// cache. Call it while the VM still runs, just before stopping it.
    pub async fn prepare_checkpoint(&self) -> Result<(), Error> {
        let (mut rd, _wr) = self
            .call(host_message::Body::PrepareCheckpoint(
                proto::PrepareCheckpoint {},
            ))
            .await?;
        done(&mut rd).await
    }

    /// The post-restore fixups, in the order `proto::Restore` documents. Call it once
    /// per restored VM, after `cont`.
    pub async fn restore(&self, restore: proto::Restore) -> Result<(), Error> {
        let (mut rd, _wr) = self.call(host_message::Body::Restore(restore)).await?;
        done(&mut rd).await
    }

    async fn call(&self, request: host_message::Body) -> Result<(Rd, Wr), Error> {
        let stream = VsockStream::connect(VsockAddr::new(self.cid, self.port)).await?;
        let (rd, mut wr) = stream.into_split();
        send(&mut wr, request).await?;
        Ok((rd, wr))
    }
}

type Rd = OwnedReadHalf;
type Wr = OwnedWriteHalf;

async fn send(wr: &mut Wr, body: host_message::Body) -> Result<(), Error> {
    let frame = encode(&HostMessage { body: Some(body) }).map_err(Error::Protocol)?;
    Ok(wr.write_all(&frame).await?)
}

/// The next message; the guest's `Error` becomes [`Error::Guest`].
async fn recv(rd: &mut Rd) -> Result<guest_message::Body, Error> {
    let mut header = [0; 4];
    rd.read_exact(&mut header).await?;
    let mut body = vec![0; body_len(header).map_err(Error::Protocol)?];
    rd.read_exact(&mut body).await?;
    let msg = GuestMessage::decode(body.as_slice()).map_err(|e| Error::Protocol(e.to_string()))?;
    match msg.body {
        Some(guest_message::Body::Error(e)) => Err(Error::Guest(e.message)),
        Some(body) => Ok(body),
        None => Err(Error::Protocol("message without a body".into())),
    }
}

async fn done(rd: &mut Rd) -> Result<(), Error> {
    match recv(rd).await? {
        guest_message::Body::Done(_) => Ok(()),
        other => Err(unexpected(&other)),
    }
}

fn unexpected(body: &guest_message::Body) -> Error {
    Error::Protocol(format!("unexpected answer {body:?}"))
}

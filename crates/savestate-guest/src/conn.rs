//! One host connection: frames in and out over an orbd guest transport session. One
//! thread reads at a time; any thread may send.

use orbd_guest::transport::Session;
use prost::Message;
use savestate_guest_proto::proto::{GuestMessage, HostMessage, guest_message, host_message};
use savestate_guest_proto::{body_len, encode};
use std::sync::{Arc, Mutex, PoisonError};

pub struct Conn {
    session: Arc<dyn Session>,
    /// Bytes read but not yet decoded.
    buf: Mutex<Vec<u8>>,
    /// Keeps frames from different threads whole.
    write: Mutex<()>,
}

const READ: usize = 64 * 1024;

impl Conn {
    pub fn new(session: Arc<dyn Session>) -> Self {
        Self {
            session,
            buf: Mutex::new(Vec::new()),
            write: Mutex::new(()),
        }
    }

    /// The next message, or `None` once the host has closed the connection.
    pub fn recv(&self) -> Result<Option<host_message::Body>, String> {
        let mut buf = self.buf.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if buf.len() >= 4 {
                let len = body_len([buf[0], buf[1], buf[2], buf[3]])?;
                if buf.len() >= 4 + len {
                    let msg = HostMessage::decode(&buf[4..4 + len])
                        .map_err(|e| format!("undecodable frame: {e}"))?;
                    buf.drain(..4 + len);
                    return msg
                        .body
                        .map(Some)
                        .ok_or_else(|| "message without a body".into());
                }
            }
            let filled = buf.len();
            buf.resize(filled + READ, 0);
            let n = self.session.read(&mut buf[filled..]);
            buf.truncate(filled + *n.as_ref().unwrap_or(&0));
            match n {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(e) => return Err(format!("reading from the host: {e}")),
            }
        }
    }

    pub fn send(&self, body: guest_message::Body) -> Result<(), String> {
        let frame = encode(&GuestMessage { body: Some(body) })?;
        let _guard = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        self.session
            .write_all(&frame)
            .map_err(|e| format!("writing to the host: {e}"))
    }

    /// Ends the connection; a thread blocked in [`Conn::recv`] sees its end.
    pub fn close(&self) {
        self.session.close();
    }
}

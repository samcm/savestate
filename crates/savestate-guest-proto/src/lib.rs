//! The savestate guest protocol (`savestate.guest.v1`, see
//! `proto/savestate/guest/v1/guest.proto`): what the host asks of `savestate-guest`
//! inside a VM (exec, file copy, the pre-checkpoint hook and the post-restore fixups),
//! its framing, and with feature `client` (on by default) the host side, [`Client`].

#[expect(clippy::doc_markdown, reason = "prost writes these doc comments")]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/savestate.guest.v1.rs"));
}

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::{Client, Error, ExitStatus};

/// The guest agent's vsock port.
pub const PORT: u32 = 1024;

/// Largest frame body either side sends or accepts.
pub const MAX_FRAME: usize = 1024 * 1024;

/// Payload size of the `Stdin`, `Output` and `Chunk` messages each side sends.
pub const CHUNK: usize = 256 * 1024;

/// `msg` as one frame: u32 big-endian length, then the protobuf body. Fails for a
/// message larger than [`MAX_FRAME`], which the other side would refuse.
pub fn encode(msg: &impl prost::Message) -> Result<Vec<u8>, String> {
    let body = msg.encode_to_vec();
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&len| len as usize <= MAX_FRAME)
        .ok_or_else(|| format!("message of {} bytes exceeds {MAX_FRAME}", body.len()))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// The body length a frame header announces, if it is within [`MAX_FRAME`].
pub fn body_len(header: [u8; 4]) -> Result<usize, String> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(format!("frame of {len} bytes exceeds {MAX_FRAME}"));
    }
    Ok(len)
}

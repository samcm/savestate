//! `ReadFile` and `WriteFile`: one regular file, streamed in chunks.

use crate::conn::Conn;
use savestate_guest_proto::CHUNK;
use savestate_guest_proto::proto::{Chunk, Done, WriteFile, guest_message, host_message};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn read(conn: &Conn, path: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut buf = vec![0; CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{path}: {e}")),
        };
        conn.send(guest_message::Body::Chunk(Chunk {
            data: buf[..n].to_vec(),
            eof: n == 0,
        }))?;
        if n == 0 {
            return Ok(());
        }
    }
}

/// Receives the file into a temporary file next to `path`, then renames it into
/// place. The host's chunks are consumed up to `eof` even when writing failed, so it
/// gets the error as the answer.
pub fn write(conn: &Conn, w: &WriteFile) -> Result<(), String> {
    let path = Path::new(&w.path);
    let mode = if w.mode == 0 { 0o644 } else { w.mode & 0o7777 };
    let mut tmp = Temp::create(path, mode).map_err(|e| format!("{}: {e}", w.path));
    loop {
        match conn.recv()? {
            Some(host_message::Body::Chunk(c)) => {
                if let Ok(t) = &mut tmp
                    && let Err(e) = t.file.write_all(&c.data)
                {
                    tmp = Err(format!("{}: {e}", w.path));
                }
                if c.eof {
                    break;
                }
            }
            Some(_) => return Err("write_file: expected a chunk".into()),
            None => {
                return Err(format!(
                    "{}: the host left before the end of the file",
                    w.path
                ));
            }
        }
    }
    tmp?.commit(path).map_err(|e| format!("{}: {e}", w.path))?;
    conn.send(guest_message::Body::Done(Done {}))
}

/// A temporary file in the target's directory, removed unless committed.
struct Temp {
    path: PathBuf,
    file: File,
}

impl Temp {
    fn create(target: &Path, mode: u32) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if !target.is_absolute() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "not an absolute path",
            ));
        }
        let name = target.file_name().ok_or(ErrorKind::InvalidInput)?;
        let path = target.with_file_name(format!(
            ".{}.savestate-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)?;
        let temp = Self { path, file };
        // Exactly `mode`, whatever the umask.
        temp.file.set_permissions(PermissionsExt::from_mode(mode))?;
        Ok(temp)
    }

    fn commit(self, target: &Path) -> std::io::Result<()> {
        self.file.sync_all()?;
        std::fs::rename(&self.path, target)?;
        File::open(target.parent().unwrap_or(Path::new("/")))?.sync_all()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        // After a commit the name is gone and this fails harmlessly.
        let _ = std::fs::remove_file(&self.path);
    }
}

//! `ReadFile` and `WriteFile`: one regular file, or a directory as a tar stream,
//! in chunks.

use crate::conn::Conn;
use savestate_guest_proto::CHUNK;
use savestate_guest_proto::proto::{Chunk, Done, FileInfo, WriteFile, guest_message, host_message};
use std::fs::{DirBuilder, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn read(conn: &Conn, path: &str) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    conn.send(guest_message::Body::FileInfo(FileInfo {
        directory: meta.is_dir(),
        mode: meta.permissions().mode() & 0o7777,
    }))?;
    let mut out = Chunks::new(conn);
    if meta.is_dir() {
        let mut tar = tar::Builder::new(&mut out);
        tar.follow_symlinks(false);
        tar.append_dir_all(".", path)
            .and_then(|()| tar.finish())
            .map_err(|e| format!("{path}: {e}"))?;
    } else {
        let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
        std::io::copy(&mut file, &mut out).map_err(|e| format!("{path}: {e}"))?;
    }
    out.finish()
}

/// Receives the file into a temporary file next to `path`, then renames it into
/// place; or with `directory`, unpacks the tar stream into `path`. The host's chunks
/// are consumed up to `eof` even when writing failed, so it gets the error as the
/// answer.
pub fn write(conn: &Conn, w: &WriteFile) -> Result<(), String> {
    let path = Path::new(&w.path);
    let mut chunks = Received::new(conn);
    let written = if w.directory {
        unpack(path, &mut chunks)
    } else {
        receive(path, w.mode, &mut chunks)
    };
    // What is left after a failure, or the padding after a tar stream's end.
    chunks.drain()?;
    written.map_err(|e| format!("{}: {e}", w.path))?;
    conn.send(guest_message::Body::Done(Done {}))
}

fn receive(path: &Path, mode: u32, chunks: &mut Received<'_>) -> std::io::Result<()> {
    let mode = if mode == 0 { 0o644 } else { mode & 0o7777 };
    let mut tmp = Temp::create(path, mode)?;
    std::io::copy(chunks, &mut tmp.file)?;
    if !chunks.eof {
        return Err(std::io::Error::other(
            "the host left before the end of the file",
        ));
    }
    tmp.commit(path)
}

fn unpack(path: &Path, chunks: &mut Received<'_>) -> std::io::Result<()> {
    absolute(path)?;
    mkdirs(path)?;
    let mut tar = tar::Archive::new(chunks);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_overwrite(true);
    tar.unpack(path)
}

fn absolute(path: &Path) -> std::io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "not an absolute path",
        ))
    }
}

/// `path` and its missing parents, mode 0755.
fn mkdirs(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o755).create(path)
}

/// Sends what is written as `Chunk`s; [`Chunks::finish`] sends the `eof` chunk.
struct Chunks<'a> {
    conn: &'a Conn,
    buf: Vec<u8>,
}

impl<'a> Chunks<'a> {
    fn new(conn: &'a Conn) -> Self {
        Self {
            conn,
            buf: Vec::with_capacity(CHUNK),
        }
    }

    fn send(&mut self, eof: bool) -> Result<(), String> {
        let data = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.conn
            .send(guest_message::Body::Chunk(Chunk { data, eof }))
    }

    fn finish(mut self) -> Result<(), String> {
        if !self.buf.is_empty() {
            self.send(false)?;
        }
        self.send(true)
    }
}

impl Write for Chunks<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = data.len().min(CHUNK - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.send(false).map_err(std::io::Error::other)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads the host's `Chunk`s up to the `eof` one.
struct Received<'a> {
    conn: &'a Conn,
    data: Vec<u8>,
    at: usize,
    eof: bool,
}

impl<'a> Received<'a> {
    fn new(conn: &'a Conn) -> Self {
        Self {
            conn,
            data: Vec::new(),
            at: 0,
            eof: false,
        }
    }

    /// Reads and drops the rest, up to the `eof` chunk.
    fn drain(&mut self) -> Result<(), String> {
        std::io::copy(self, &mut std::io::sink()).map_err(|e| e.to_string())?;
        if self.eof {
            Ok(())
        } else {
            Err("the host left before the end of the file".into())
        }
    }
}

impl Read for Received<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.data.len() {
            if self.eof {
                return Ok(0);
            }
            match self.conn.recv().map_err(std::io::Error::other)? {
                Some(host_message::Body::Chunk(c)) => {
                    self.data = c.data;
                    self.at = 0;
                    self.eof = c.eof;
                }
                Some(_) => return Err(std::io::Error::other("write_file: expected a chunk")),
                // The caller sees an early end as `eof` still false.
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// A temporary file in the target's directory, removed unless committed.
struct Temp {
    path: PathBuf,
    file: File,
}

impl Temp {
    fn create(target: &Path, mode: u32) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        absolute(target)?;
        let name = target.file_name().ok_or(ErrorKind::InvalidInput)?;
        if let Some(parent) = target.parent() {
            mkdirs(parent)?;
        }
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

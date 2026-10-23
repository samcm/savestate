//! A checkpoint's files in an S3-compatible object store: export from a node's state
//! directory, download onto another node, and garbage collection. The layout and
//! the protocol are "Object stores" in `docs/design.md`:
//!
//! ```text
//! <prefix>/files/<path>/<sha256>      a chunk of the file's data (at most CHUNK bytes)
//! <prefix>/files/<path>/index.json    size, data extents, chunk digests; written last
//! <prefix>/exports/<id>.json          an export in progress: the files it references
//! <prefix>/manifests/<id>.json        the checkpoint's manifest and files; written last
//! ```
//!
//! `<path>` is a file's path relative to the state directory, which names one
//! immutable file, so files are shared by every checkpoint that references them. A
//! file's data is its allocated extents: holes are neither stored nor moved.

mod layout;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt as _, TryStreamExt as _};
use object_store::{
    ClientOptions, ObjectStore, ObjectStoreExt as _, PutPayload, RetryConfig, aws::AmazonS3Builder,
    path::Path,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;

use layout::Layout;

/// Largest chunk of a file's data, one object each.
pub const CHUNK: u64 = 16 << 20;

/// Chunks moving at once per export or download.
const PARALLEL: usize = 16;

/// How long after writing its export record an export trusts that objects already
/// in the store stay there. Longer than [`COLLECT_DEADLINE`] plus a request's
/// timeout: a collection that listed the records before this one existed has
/// stopped deleting by then.
pub const SETTLE: Duration = Duration::from_secs(30);

/// How long after it starts listing records a collection may still delete files.
pub const COLLECT_DEADLINE: Duration = Duration::from_secs(10);

/// Request timeout of a collection, which neither retries: a delete that is late
/// must fail rather than land after [`SETTLE`].
const COLLECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("object store: {0}")]
    Store(#[from] object_store::Error),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("corrupt store object: {0}")]
    Corrupt(String),
    #[error("listing live checkpoints: {0}")]
    Live(Box<dyn std::error::Error + Send + Sync>),
    #[error("{0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Store credentials: an access key, and a session token for temporary ones.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// Bytes of a transfer: `total` once known, `done` (copied, or found already at the
/// destination) as it goes. Clones share the counts.
#[derive(Clone, Debug, Default)]
pub struct Progress(Arc<Counts>);

#[derive(Debug, Default)]
struct Counts {
    done: AtomicU64,
    total: AtomicU64,
}

impl Progress {
    pub fn done(&self) -> u64 {
        self.0.done.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.0.total.load(Ordering::Relaxed)
    }

    fn add(&self, bytes: u64) {
        self.0.done.fetch_add(bytes, Ordering::Relaxed);
    }

    fn set_total(&self, bytes: u64) {
        self.0.total.store(bytes, Ordering::Relaxed);
    }
}

/// What a transfer moved: bytes of data copied, and bytes it found already there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moved {
    pub copied: u64,
    pub present: u64,
}

/// `manifests/<id>.json`: the checkpoint's own manifest, kept as it was written,
/// and the files it needs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub checkpoint: serde_json::Value,
    pub files: Vec<String>,
}

/// `exports/<id>.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    files: Vec<String>,
}

/// `files/<path>/index.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    size: u64,
    extents: Vec<(u64, u64)>,
    chunks: Vec<String>,
}

/// What a garbage collection deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Manifests and export records of checkpoints that are gone.
    pub records: usize,
    /// Files no record names.
    pub files: usize,
    /// Whether it stopped at the deadline with files left to delete.
    pub stopped: bool,
}

/// One store, under one prefix.
pub struct Remote {
    /// For transfers: retries, generous timeouts.
    data: Arc<dyn ObjectStore>,
    /// For garbage collection: no retries, short timeouts.
    collector: Arc<dyn ObjectStore>,
    prefix: String,
}

impl Remote {
    /// An S3-compatible store at `endpoint` (`http://` allowed), with path-style
    /// requests.
    pub fn s3(
        endpoint: &str,
        bucket: &str,
        region: &str,
        prefix: &str,
        credentials: &Credentials,
    ) -> Result<Self, Error> {
        // TLS for the HTTP client; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let build =
            |retry: RetryConfig, timeout: Duration| -> Result<Arc<dyn ObjectStore>, Error> {
                let mut builder = AmazonS3Builder::new()
                    .with_endpoint(endpoint)
                    .with_bucket_name(bucket)
                    .with_region(region)
                    .with_access_key_id(&credentials.access_key_id)
                    .with_secret_access_key(&credentials.secret_access_key)
                    .with_virtual_hosted_style_request(false)
                    .with_retry(retry)
                    .with_client_options(
                        ClientOptions::new()
                            .with_allow_http(endpoint.starts_with("http://"))
                            .with_timeout(timeout),
                    );
                if let Some(token) = &credentials.session_token {
                    builder = builder.with_token(token);
                }
                Ok(Arc::new(builder.build()?))
            };
        let collector = RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        };
        Ok(Self {
            data: build(RetryConfig::default(), Duration::from_secs(60))?,
            collector: build(collector, COLLECT_TIMEOUT)?,
            prefix: prefix.trim_matches('/').to_owned(),
        })
    }

    fn key(&self, relative: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(relative)
        } else {
            Path::from(format!("{}/{relative}", self.prefix))
        }
    }

    /// The part of `key` under `<prefix>/<dir>/`.
    fn under<'a>(&self, dir: &str, key: &'a str) -> Option<&'a str> {
        let rest = if self.prefix.is_empty() {
            key
        } else {
            key.strip_prefix(self.prefix.as_str())?.strip_prefix('/')?
        };
        rest.strip_prefix(dir)?.strip_prefix('/')
    }

    /// Whether checkpoint `id` is complete in the store.
    pub async fn contains(&self, id: &str) -> Result<bool, Error> {
        exists(&*self.data, &self.key(&format!("manifests/{id}.json"))).await
    }

    /// The manifest of checkpoint `id`, if it is complete in the store.
    pub async fn manifest(&self, id: &str) -> Result<Option<Manifest>, Error> {
        let key = self.key(&format!("manifests/{id}.json"));
        match self.data.get(&key).await {
            Ok(object) => Ok(Some(json(&key, &object.bytes().await?)?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Exports checkpoint `id`: `files` (paths relative to `root`, the state
    /// directory) and its own `manifest`. Files go in the order given; put the
    /// checkpoint's own (newest) files first. Resumes what an interrupted export of
    /// the same checkpoint left; an export that does not finish leaves no manifest.
    pub async fn export(
        &self,
        id: &str,
        root: &FsPath,
        files: &[String],
        manifest: &serde_json::Value,
        progress: &Progress,
    ) -> Result<Moved, Error> {
        let mut local = Vec::with_capacity(files.len());
        for path in files {
            let full = root.join(path);
            let opened = tokio::task::spawn_blocking(move || -> Result<_, Error> {
                let file = File::open(&full).map_err(|source| io(&full, source))?;
                let layout = Layout::of(&file).map_err(|source| io(&full, source))?;
                Ok((Arc::new(file), Arc::new(layout)))
            })
            .await??;
            local.push(opened);
        }
        let total = local.iter().map(|(_, layout)| layout.len).sum();
        progress.set_total(total);
        let mut moved = Moved::default();
        if self.contains(id).await? {
            progress.add(total);
            moved.present = total;
            return Ok(moved);
        }

        let record = self.key(&format!("exports/{id}.json"));
        put_json(
            &*self.data,
            &record,
            &Record {
                files: files.to_vec(),
            },
        )
        .await?;
        // Until now a collection may still be deleting what this export would reuse.
        tokio::time::sleep(SETTLE).await;

        for (path, (file, layout)) in files.iter().zip(local) {
            let file_moved = self
                .upload(&root.join(path), path, file, layout, progress)
                .await?;
            moved.copied += file_moved.copied;
            moved.present += file_moved.present;
        }

        put_json(
            &*self.data,
            &self.key(&format!("manifests/{id}.json")),
            &Manifest {
                checkpoint: manifest.clone(),
                files: files.to_vec(),
            },
        )
        .await?;
        self.data.delete(&record).await?;
        Ok(moved)
    }

    /// Uploads one file of an export, unless the store has it: the chunks it lacks,
    /// then the index.
    async fn upload(
        &self,
        full: &FsPath,
        path: &str,
        file: Arc<File>,
        layout: Arc<Layout>,
        progress: &Progress,
    ) -> Result<Moved, Error> {
        let mut moved = Moved::default();
        let index = self.key(&format!("files/{path}/index.json"));
        if exists(&*self.data, &index).await? {
            progress.add(layout.len);
            moved.present = layout.len;
            return Ok(moved);
        }
        let dir = self.key(&format!("files/{path}"));
        let present: HashSet<String> = self
            .data
            .list(Some(&dir))
            .map_ok(|meta| meta.location.filename().unwrap_or_default().to_owned())
            .try_collect()
            .await?;
        let present = Arc::new(present);
        let chunks: Vec<(String, u64, bool)> = futures::stream::iter(0..layout.chunks())
            .map(|k| {
                let (file, layout, present) = (file.clone(), layout.clone(), present.clone());
                let (store, dir, full) = (self.data.clone(), dir.clone(), full.to_owned());
                let progress = progress.clone();
                async move {
                    let (start, end) = layout.chunk(k);
                    let data = tokio::task::spawn_blocking(move || {
                        layout
                            .read(&file, start, end)
                            .map_err(|source| io(&full, source))
                    })
                    .await??;
                    let digest = hex::encode(Sha256::digest(&data));
                    let new = !present.contains(&digest);
                    if new {
                        store
                            .put(&dir.join(digest.as_str()), PutPayload::from(data))
                            .await?;
                    }
                    progress.add(end - start);
                    Ok::<_, Error>((digest, end - start, new))
                }
            })
            .buffered(PARALLEL)
            .try_collect()
            .await?;
        for (_, bytes, new) in &chunks {
            if *new {
                moved.copied += bytes;
            } else {
                moved.present += bytes;
            }
        }
        put_json(
            &*self.data,
            &index,
            &Index {
                size: layout.size,
                extents: layout.extents.clone(),
                chunks: chunks.into_iter().map(|(digest, _, _)| digest).collect(),
            },
        )
        .await?;
        Ok(moved)
    }

    /// Downloads each `(path, target)`: the store's file `path` to the local file
    /// `target`, through `<target>.part`, fsynced and renamed into place read-only.
    /// Every chunk is checked against its digest before it is written.
    pub async fn download(
        &self,
        files: &[(String, PathBuf)],
        progress: &Progress,
    ) -> Result<Moved, Error> {
        let mut indexes = Vec::with_capacity(files.len());
        for (path, _) in files {
            let key = self.key(&format!("files/{path}/index.json"));
            let index: Index = match self.data.get(&key).await {
                Ok(object) => json(&key, &object.bytes().await?)?,
                Err(object_store::Error::NotFound { .. }) => {
                    return Err(Error::Corrupt(format!("{path} is not in the store")));
                }
                Err(error) => return Err(error.into()),
            };
            let layout = Layout::checked(index.size, index.extents)?;
            if u64::try_from(index.chunks.len()).ok() != Some(layout.chunks()) {
                return Err(Error::Corrupt(format!(
                    "{key}: {} chunks for {} bytes of data",
                    index.chunks.len(),
                    layout.len
                )));
            }
            indexes.push((Arc::new(layout), index.chunks));
        }
        let total = indexes.iter().map(|(layout, _)| layout.len).sum();
        progress.set_total(total);

        for ((path, target), (layout, chunks)) in files.iter().zip(indexes) {
            let mut part = target.clone().into_os_string();
            part.push(".part");
            let part = PathBuf::from(part);
            let result = self
                .download_file(path, &part, layout, &chunks, progress)
                .await;
            if result.is_err() {
                let _ = std::fs::remove_file(&part);
            }
            result?;
            std::fs::rename(&part, target).map_err(|source| io(target, source))?;
            if let Some(dir) = target.parent() {
                File::open(dir)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|source| io(dir, source))?;
            }
        }
        Ok(Moved {
            copied: total,
            present: 0,
        })
    }

    async fn download_file(
        &self,
        path: &str,
        part: &FsPath,
        layout: Arc<Layout>,
        chunks: &[String],
        progress: &Progress,
    ) -> Result<(), Error> {
        let file = File::create(part).map_err(|source| io(part, source))?;
        file.set_len(layout.size)
            .map_err(|source| io(part, source))?;
        let file = Arc::new(file);
        let dir = self.key(&format!("files/{path}"));
        futures::stream::iter(chunks.iter().cloned().enumerate())
            .map(|(k, digest)| {
                let (file, layout, store) = (file.clone(), layout.clone(), self.data.clone());
                let (key, part, progress) = (
                    dir.clone().join(digest.as_str()),
                    part.to_owned(),
                    progress.clone(),
                );
                async move {
                    let (start, end) = layout.chunk(k as u64);
                    let data = store.get(&key).await?.bytes().await?;
                    if data.len() as u64 != end - start
                        || hex::encode(Sha256::digest(&data)) != digest
                    {
                        return Err(Error::Corrupt(format!(
                            "{key}: {} bytes that do not match their digest",
                            data.len()
                        )));
                    }
                    tokio::task::spawn_blocking(move || {
                        layout
                            .write(&file, start, &data)
                            .map_err(|source| io(&part, source))
                    })
                    .await??;
                    progress.add(end - start);
                    Ok(())
                }
            })
            .buffer_unordered(PARALLEL)
            .try_collect::<()>()
            .await?;
        let part = part.to_owned();
        tokio::task::spawn_blocking(move || {
            file.sync_all().map_err(|source| io(&part, source))?;
            let mut permissions = file
                .metadata()
                .map_err(|source| io(&part, source))?
                .permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o444);
            std::fs::set_permissions(&part, permissions).map_err(|source| io(&part, source))
        })
        .await??;
        Ok(())
    }

    /// Deletes what no live checkpoint needs: manifests and export records whose
    /// checkpoint (`live` lists the ids that exist, read after this lists the
    /// records) is gone, then files no remaining record names, until
    /// [`COLLECT_DEADLINE`] after it started listing the records. `cache` keeps the
    /// files of records between collections (records do not change).
    pub async fn collect<F, Fut>(
        &self,
        live: F,
        cache: &mut HashMap<String, Vec<String>>,
    ) -> Result<Collected, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<BTreeSet<String>, Box<dyn std::error::Error + Send + Sync>>>,
    {
        let store = &*self.collector;
        // Files first: one uploaded after this listing is not considered.
        let mut files: BTreeMap<String, Vec<Path>> = BTreeMap::new();
        let mut listing = store.list(Some(&self.key("files")));
        while let Some(meta) = listing.try_next().await? {
            let Some((path, _)) = self
                .under("files", meta.location.as_ref())
                .and_then(|rest| rest.rsplit_once('/'))
            else {
                continue;
            };
            files
                .entry(path.to_owned())
                .or_default()
                .push(meta.location);
        }

        // Then the records, then the checkpoints.
        let deadline = Instant::now() + COLLECT_DEADLINE;
        let mut records = Vec::new();
        for dir in ["exports", "manifests"] {
            let mut listing = store.list(Some(&self.key(dir)));
            while let Some(meta) = listing.try_next().await? {
                let id = self
                    .under(dir, meta.location.as_ref())
                    .and_then(|name| name.strip_suffix(".json"))
                    .map(str::to_owned);
                if let Some(id) = id {
                    records.push((id, meta.location));
                }
            }
        }
        let live = live().await.map_err(Error::Live)?;

        let mut collected = Collected::default();
        let (kept, gone): (Vec<_>, Vec<_>) =
            records.into_iter().partition(|(id, _)| live.contains(id));
        for (id, key) in gone {
            tracing::info!(checkpoint = id, %key, "deleting the record of a checkpoint that is gone");
            match store.delete(&key).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            cache.remove(key.as_ref());
            collected.records += 1;
        }

        let referenced = self.referenced(&kept, cache).await?;

        for (path, mut objects) in files {
            if referenced.contains(&path) {
                continue;
            }
            if Instant::now() >= deadline {
                collected.stopped = true;
                break;
            }
            tracing::info!(
                path,
                objects = objects.len(),
                "deleting a file no checkpoint uses"
            );
            // The index first: a file without one is incomplete, never trusted.
            objects.sort_by_key(|key| key.filename() != Some("index.json"));
            for batch in objects.chunks(1000) {
                if Instant::now() >= deadline {
                    collected.stopped = true;
                    return Ok(collected);
                }
                let keys: Vec<_> = batch.iter().cloned().map(Ok).collect();
                let mut deleted = store.delete_stream(futures::stream::iter(keys).boxed());
                while let Some(result) = deleted.next().await {
                    match result {
                        Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            collected.files += 1;
        }
        Ok(collected)
    }
    /// The files the records `kept` name, reading the records not in `cache`.
    async fn referenced(
        &self,
        kept: &[(String, Path)],
        cache: &mut HashMap<String, Vec<String>>,
    ) -> Result<BTreeSet<String>, Error> {
        let missing: Vec<Path> = kept
            .iter()
            .map(|(_, key)| key)
            .filter(|key| !cache.contains_key(key.as_ref()))
            .cloned()
            .collect();
        let read: Vec<(Path, Vec<String>)> = futures::stream::iter(missing)
            .map(|key| {
                let store = self.collector.clone();
                let manifest = self.under("manifests", key.as_ref()).is_some();
                async move {
                    match store.get(&key).await {
                        Ok(object) => {
                            let bytes = object.bytes().await?;
                            let files = if manifest {
                                json::<Manifest>(&key, &bytes)?.files
                            } else {
                                json::<Record>(&key, &bytes)?.files
                            };
                            Ok(Some((key, files)))
                        }
                        // Finished (an export record) or deleted meanwhile.
                        Err(object_store::Error::NotFound { .. }) => Ok(None),
                        Err(error) => Err(Error::from(error)),
                    }
                }
            })
            .buffer_unordered(PARALLEL)
            .try_filter_map(|found| async move { Ok(found) })
            .try_collect()
            .await?;
        for (key, files) in read {
            cache.insert(key.to_string(), files);
        }
        let mut referenced = BTreeSet::new();
        for (_, key) in kept {
            if let Some(files) = cache.get(key.as_ref()) {
                referenced.extend(files.iter().cloned());
            }
        }
        Ok(referenced)
    }
}

async fn exists(store: &dyn ObjectStore, key: &Path) -> Result<bool, Error> {
    match store.head(key).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn put_json<T: Serialize>(
    store: &dyn ObjectStore,
    key: &Path,
    value: &T,
) -> Result<(), Error> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| Error::Corrupt(format!("{key}: {error}")))?;
    store.put(key, PutPayload::from(bytes)).await?;
    Ok(())
}

fn json<T: serde::de::DeserializeOwned>(key: &Path, bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|error| Error::Corrupt(format!("{key}: {error}")))
}

fn io(path: &FsPath, source: std::io::Error) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source,
    }
}

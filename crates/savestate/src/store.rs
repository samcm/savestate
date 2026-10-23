//! A node's state directory (`stateDir`; see "Storage layout" in `docs/design.md`):
//!
//! ```text
//! images/<hex>.qcow2              base images by manifest digest, read-only
//! layers/<sandbox UID>-<n>.qcow2  disk layers; a sandbox's nth layer
//! checkpoints/<UID>/              published checkpoint (Checkpoint UID): manifest.json, memory
//! checkpoints/<UID>.tmp/          save or import in progress; complete once manifest.json exists
//! trash/                          delete-by-rename
//! ```
//!
//! Disk layers name their backing file relative to their own directory
//! (`<id>.qcow2`, `../images/<hex>.qcow2`), so chains stay valid wherever the state
//! directory is mounted. The node agent owns the directory; launchers (as
//! [`QEMU_UID`]) create layers and write checkpoint directories the node agent made
//! for them, and the node agent publishes them.

use std::{
    collections::BTreeSet,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use orbd_qemu::qmp::FileMigration;
use orbd_vm::{
    checkpoint::{CheckpointStore, MANIFEST},
    fsutil,
};
use savestate_api::Fingerprint;
use serde::{Deserialize, Serialize};

/// User and group QEMU and the launcher run as once the VM pod's network is set up.
pub const QEMU_UID: u32 = 65532;
pub const QEMU_GID: u32 = 65532;

/// The memory file in a checkpoint directory.
pub const MEMORY: &str = "memory";

/// What a checkpoint holds and what restoring it must run, written last into its
/// directory (its presence marks the save complete).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub namespace: String,
    /// The Checkpoint's name.
    pub name: String,
    pub sandbox: String,
    /// Checkpoint the sandbox was started from.
    pub parent: Option<String>,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub disk_bytes: u64,
    /// The guest's wall clock just before the save, in nanoseconds since the epoch.
    pub guest_time_unix_ns: Option<i64>,
    /// The sandbox image the chain's base image was pulled from, by digest
    /// (`…@sha256:<hex>`). Stores do not hold the base image; an import pulls it from
    /// here.
    pub image: String,
    /// The disk, base image first, the checkpoint's own (frozen) layer last; paths
    /// relative to the state directory.
    pub chain: Vec<String>,
    /// QEMU's guest-visible arguments, reused verbatim by restores.
    pub machine_args: Vec<String>,
    pub migration: FileMigration,
    /// Bytes the memory file occupies on disk.
    pub memory_bytes: u64,
    /// Bytes the checkpoint's own layer occupies on disk.
    pub layer_bytes: u64,
    pub fingerprint: Fingerprint,
}

pub struct Store {
    root: PathBuf,
    checkpoints: CheckpointStore,
}

/// What the state directory holds, listed before the API objects that keep it:
/// anything created after the listing is not considered for removal.
#[derive(Debug, Default)]
pub struct Inventory {
    /// Image digests (hex).
    pub images: Vec<String>,
    /// Layer ids (`<sandbox UID>-<n>`).
    pub layers: Vec<String>,
    /// Published checkpoints (UIDs).
    pub checkpoints: Vec<String>,
    /// Saves in progress (UIDs).
    pub saves: Vec<String>,
}

/// The objects that exist, by UID, and the images sandboxes boot from.
#[derive(Debug, Default)]
pub struct Live {
    pub sandboxes: BTreeSet<String>,
    pub checkpoints: BTreeSet<String>,
    /// Image digests (hex) of sandboxes started from images.
    pub images: BTreeSet<String>,
    /// Paths (as chains name them) that imports in progress are writing.
    pub files: BTreeSet<String>,
}

/// Bytes on disk per kind, and free bytes on the state directory's filesystem.
#[derive(Debug, Default)]
pub struct Usage {
    pub images: u64,
    pub layers: u64,
    pub checkpoints: u64,
    pub free: u64,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        let checkpoints = CheckpointStore::new(root.join("checkpoints"), root.join("trash"));
        Self { root, checkpoints }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn image(&self, hex: &str) -> PathBuf {
        self.root.join(image_path(hex))
    }

    pub fn layers(&self) -> PathBuf {
        self.root.join("layers")
    }

    pub fn layer(&self, id: &str) -> PathBuf {
        self.root.join(layer_path(id))
    }

    pub fn checkpoints(&self) -> &CheckpointStore {
        &self.checkpoints
    }

    fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }

    /// Creates the directories (layers owned by [`QEMU_UID`], who creates them),
    /// removes interrupted image pulls and empties the trash. For the node agent, at
    /// start-up.
    pub fn prepare(&self) -> io::Result<()> {
        for dir in ["images", "layers", "checkpoints", "trash"] {
            std::fs::create_dir_all(self.root.join(dir))?;
        }
        std::os::unix::fs::chown(self.layers(), Some(QEMU_UID), Some(QEMU_GID))?;
        // Image pulls and imports a restart interrupted.
        for dir in ["images", "layers"] {
            for name in list(&self.root.join(dir)) {
                if Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext == "part")
                {
                    std::fs::remove_file(self.root.join(dir).join(name))?;
                }
            }
        }
        for entry in std::fs::read_dir(self.trash())? {
            let path = entry?.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// A fresh directory for saving checkpoint `id`, writable by the launcher.
    pub fn begin_save(&self, id: &str) -> io::Result<PathBuf> {
        let tmp = self.checkpoints.fresh_tmp(id)?;
        std::os::unix::fs::chown(&tmp, Some(QEMU_UID), Some(QEMU_GID))?;
        Ok(tmp)
    }

    /// Where the save of checkpoint `id` stands, finishing what a crash interrupted:
    /// a published checkpoint, or a complete save (which is published now), gives its
    /// manifest; an incomplete save is discarded and gives `None`, as does nothing.
    pub fn settle(&self, id: &str) -> io::Result<Option<Manifest>> {
        let published = self.checkpoints.published(id);
        if !published.exists() {
            if !self.checkpoints.tmp_is_complete(id) {
                self.checkpoints.discard_tmp(id)?;
                return Ok(None);
            }
            self.checkpoints.publish(id)?;
        }
        read_manifest(&published).map(Some)
    }

    /// A published checkpoint's manifest.
    pub fn manifest(&self, id: &str) -> io::Result<Manifest> {
        read_manifest(&self.checkpoints.published(id))
    }

    pub fn inventory(&self) -> Inventory {
        let stems = |dir: &str| -> Vec<String> {
            list(&self.root.join(dir))
                .into_iter()
                .filter_map(|name| name.strip_suffix(".qcow2").map(str::to_owned))
                .collect()
        };
        let (checkpoints, saves) = self.checkpoints.list();
        Inventory {
            images: stems("images"),
            layers: stems("layers"),
            checkpoints,
            saves,
        }
    }

    /// Removes what `inventory` lists and no live object needs: checkpoints (and saves)
    /// whose object is gone, then layers and images that neither a live sandbox nor a
    /// remaining checkpoint's chain uses. Returns the paths removed. Layers and images
    /// are kept whole when a remaining checkpoint's manifest cannot be read.
    pub fn collect(&self, inventory: &Inventory, live: &Live) -> io::Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        let mut chains = BTreeSet::new();
        let mut chains_known = true;
        for id in &inventory.checkpoints {
            let dir = self.checkpoints.published(id);
            if !live.checkpoints.contains(id) {
                self.checkpoints.discard(id)?;
                removed.push(dir);
                continue;
            }
            match read_manifest(&dir) {
                Ok(manifest) => chains.extend(manifest.chain),
                Err(error) => {
                    tracing::warn!(checkpoint = id, %error, "unreadable manifest; keeping all layers");
                    chains_known = false;
                }
            }
        }
        for id in &inventory.saves {
            let dir = self.checkpoints.tmp(id);
            if !live.checkpoints.contains(id) {
                self.checkpoints.discard_tmp(id)?;
                removed.push(dir);
                continue;
            }
            // A complete save is published soon; keep what it needs.
            if let Ok(manifest) = read_manifest(&dir) {
                chains.extend(manifest.chain);
            }
        }
        if !chains_known {
            return Ok(removed);
        }
        for id in &inventory.layers {
            let sandbox = id.rsplit_once('-').map_or(id.as_str(), |(uid, _)| uid);
            let path = layer_path(id);
            if live.sandboxes.contains(sandbox)
                || chains.contains(&path)
                || live.files.contains(&path)
            {
                continue;
            }
            let path = self.layer(id);
            fsutil::remove_via_trash(&path, &self.trash())?;
            removed.push(path);
        }
        for hex in &inventory.images {
            let path = image_path(hex);
            if live.images.contains(hex) || chains.contains(&path) || live.files.contains(&path) {
                continue;
            }
            let path = self.image(hex);
            // Used lately (its modification time is its last use): a sandbox deleted and
            // created again from the same image should not pay its pull twice.
            let idle = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .map(|used| used.elapsed().unwrap_or_default());
            if idle.is_ok_and(|idle| idle < IMAGE_GRACE) {
                continue;
            }
            fsutil::remove_via_trash(&path, &self.trash())?;
            removed.push(path);
        }
        Ok(removed)
    }

    /// Disk use per kind (allocated bytes, so sparse files count what they occupy).
    pub fn usage(&self) -> Usage {
        Usage {
            images: allocated(&self.root.join("images")),
            layers: allocated(&self.layers()),
            checkpoints: allocated(self.checkpoints.dir()),
            free: fsutil::free_bytes(&self.root).unwrap_or(0),
        }
    }
}

/// How long an image nothing uses any more stays on the node after its last use.
pub const IMAGE_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Records that an image is in use now (its modification time), for
/// [`IMAGE_GRACE`].
pub fn touch(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.set_modified(std::time::SystemTime::now())
}

/// `images/<hex>.qcow2`, as chains name it.
pub fn image_path(hex: &str) -> String {
    format!("images/{hex}.qcow2")
}

/// `layers/<id>.qcow2`, as chains name it.
pub fn layer_path(id: &str) -> String {
    format!("layers/{id}.qcow2")
}

/// The id of a sandbox's `n`th layer.
pub fn layer_id(sandbox_uid: &str, n: u32) -> String {
    format!("{sandbox_uid}-{n}")
}

pub fn read_manifest(dir: &Path) -> io::Result<Manifest> {
    fsutil::read_json(&dir.join(MANIFEST))?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} has no {MANIFEST}", dir.display()),
        )
    })
}

/// Bytes `path` occupies on disk: a file's allocated blocks, or a tree's.
pub fn allocated(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.blocks() * 512;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| allocated(&entry.path()))
        .sum()
}

fn list(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

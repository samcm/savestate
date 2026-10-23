//! Base images, cached by digest: a sandbox's `image` is a container image whose
//! filesystem holds `/rootfs.qcow2` (the `disk` stage of `guest/rootfs/Dockerfile`, or
//! an installation's equivalent). The node agent pulls it once per node into
//! `images/<hex>.qcow2`, checks that it is a qcow2 image without a backing file, and
//! publishes it read-only by rename.

use std::{
    collections::HashMap,
    io::Read as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use oci_client::{
    Client, Reference,
    client::{ClientConfig, ClientProtocol},
    manifest::{
        IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
        OCI_IMAGE_MEDIA_TYPE, OciDescriptor,
    },
    secrets::RegistryAuth,
};
use orbd_vm::fsutil;
use savestate_api::Config;
use sha2::{Digest as _, Sha256};

use crate::store::Store;

/// The disk inside a sandbox image.
const DISK: &str = "rootfs.qcow2";
const ATTEMPTS: u32 = 3;

pub(super) struct Images {
    client: Client,
    /// Docker `config.json` credentials, by registry host.
    auths: HashMap<String, RegistryAuth>,
    dir: PathBuf,
    root: PathBuf,
    /// One pull per digest at a time.
    pulls: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Images {
    pub(super) fn new(config: &Config, store: &Store) -> anyhow::Result<Self> {
        // reqwest is built without a bundled crypto provider; use ring like kube.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let protocol = if config.images.plain_http.is_empty() {
            ClientProtocol::Https
        } else {
            ClientProtocol::HttpsExcept(config.images.plain_http.clone())
        };
        let client = Client::new(ClientConfig {
            protocol,
            platform_resolver: Some(Box::new(oci_client::client::linux_amd64_resolver)),
            ..ClientConfig::default()
        });
        let auths = match &config.images.auth {
            Some(path) => read_auths(path)?,
            None => HashMap::new(),
        };
        let dir = store.root().join("images");
        Ok(Self {
            client,
            auths,
            dir,
            root: store.root().to_owned(),
            pulls: Mutex::default(),
        })
    }

    /// Makes sure the image `reference` (`…@sha256:<hex>`) is here; returns `<hex>`.
    pub(super) async fn ensure(&self, reference: &str) -> anyhow::Result<String> {
        let hex = reference
            .rsplit_once("@sha256:")
            .map(|(_, hex)| hex.to_owned())
            .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .with_context(|| format!("image {reference} is not pinned by sha256 digest"))?;
        let path = self.root.join(crate::store::image_path(&hex));
        if path.exists() {
            return Ok(hex);
        }
        let lock = self
            .pulls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(hex.clone())
            .or_default()
            .clone();
        let _pulling = lock.lock().await;
        if path.exists() {
            return Ok(hex);
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.pull(reference, &hex, &path).await {
                Ok(()) => break,
                Err(error) if attempt < ATTEMPTS => {
                    tracing::warn!(reference, %error, attempt, "pulling an image failed; retrying");
                    tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
                }
                Err(error) => return Err(error.context(format!("pulling image {reference}"))),
            }
        }
        metrics::counter!("savestate_node_images_pulled_total").increment(1);
        Ok(hex)
    }

    async fn pull(&self, reference: &str, hex: &str, path: &Path) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let image: Reference = reference
            .parse()
            .with_context(|| format!("invalid image reference {reference}"))?;
        let auth = self
            .auths
            .get(image.resolve_registry())
            .or_else(|| self.auths.get(image.registry()))
            .cloned()
            .unwrap_or(RegistryAuth::Anonymous);
        let layers = self.layers(&image, &auth, hex).await?;
        let blob = self.dir.join(format!("{hex}.blob.part"));
        let disk = self.dir.join(format!("{hex}.qcow2.part"));
        // The last layer holding the disk wins, as in the image's filesystem.
        let mut found = false;
        for layer in layers.iter().rev() {
            let mut file = tokio::io::BufWriter::new(tokio::fs::File::create(&blob).await?);
            // Verifies the content against the layer's digest while streaming, and
            // flushes.
            self.client
                .pull_blob(&image, layer, &mut file)
                .await
                .with_context(|| format!("layer {}", layer.digest))?;
            let (blob_path, disk_path, media_type) =
                (blob.clone(), disk.clone(), layer.media_type.clone());
            found =
                tokio::task::spawn_blocking(move || extract(&blob_path, &media_type, &disk_path))
                    .await??;
            if found {
                break;
            }
        }
        let _ = tokio::fs::remove_file(&blob).await;
        ensure!(found, "the image has no /{DISK}");
        check_disk(&disk).await?;
        std::fs::set_permissions(&disk, std::fs::Permissions::from_mode(0o444))?;
        std::fs::File::open(&disk)?.sync_all()?;
        fsutil::rename_durable(&disk, path)?;
        tracing::info!(
            reference,
            seconds = started.elapsed().as_secs_f64(),
            "image cached"
        );
        Ok(())
    }

    /// The image's layers, for linux/amd64 when it is an index; manifests are checked
    /// against their digests.
    async fn layers(
        &self,
        image: &Reference,
        auth: &RegistryAuth,
        hex: &str,
    ) -> anyhow::Result<Vec<OciDescriptor>> {
        let accepted = [
            OCI_IMAGE_INDEX_MEDIA_TYPE,
            IMAGE_MANIFEST_LIST_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
            IMAGE_MANIFEST_MEDIA_TYPE,
        ];
        let (raw, _) = self
            .client
            .pull_manifest_raw(image, auth, &accepted)
            .await
            .context("fetching the manifest")?;
        ensure!(
            sha256(&raw) == hex,
            "the registry's manifest does not match the image digest"
        );
        let value: serde_json::Value = serde_json::from_slice(&raw)?;
        let manifest = if let Some(manifests) = value.get("manifests").and_then(|m| m.as_array()) {
            let digest = manifests
                .iter()
                .find(|m| {
                    m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64"
                })
                .and_then(|m| m["digest"].as_str())
                .context("the image index has no linux/amd64 image")?
                .to_owned();
            let child = Reference::with_digest(
                image.registry().to_owned(),
                image.repository().to_owned(),
                digest.clone(),
            );
            let (raw, _) = self
                .client
                .pull_manifest_raw(&child, auth, &accepted)
                .await
                .context("fetching the linux/amd64 manifest")?;
            ensure!(
                format!("sha256:{}", sha256(&raw)) == digest,
                "the registry's manifest does not match {digest}"
            );
            serde_json::from_slice::<serde_json::Value>(&raw)?
        } else {
            value
        };
        serde_json::from_value(manifest["layers"].clone()).context("the manifest's layers")
    }
}

/// Extracts [`DISK`] from the layer blob into `out`; false if the layer has none.
fn extract(blob: &Path, media_type: &str, out: &Path) -> anyhow::Result<bool> {
    let file = std::io::BufReader::new(std::fs::File::open(blob)?);
    let reader: Box<dyn std::io::Read> = if media_type.ends_with("gzip") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else if media_type.rsplit(['.', '+']).next() == Some("tar") {
        Box::new(file)
    } else {
        bail!("unsupported layer media type {media_type} (use gzip or uncompressed tar)");
    };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.strip_prefix("./").unwrap_or(&path);
        if name != Path::new(DISK) || !entry.header().entry_type().is_file() {
            continue;
        }
        let mut disk = std::fs::File::create(out)?;
        std::io::copy(&mut entry, &mut disk)?;
        disk.sync_all()?;
        return Ok(true);
    }
    // Read to the end so a truncated stream is an error, not an absent disk.
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
    Ok(false)
}

/// A base image must be a plain qcow2 image: no backing file, which could name any
/// file on the node.
async fn check_disk(disk: &Path) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("qemu-img")
        .args(["info", "--output=json", "-f", "qcow2"])
        .arg(disk)
        .output()
        .await
        .context("qemu-img info")?;
    ensure!(
        out.status.success(),
        "{DISK} is not a qcow2 image: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    ensure!(
        info.get("backing-filename").is_none(),
        "{DISK} has a backing file"
    );
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Credentials from a Docker `config.json` (`auths.<registry>.auth`, or `username`
/// and `password`).
fn read_auths(path: &Path) -> anyhow::Result<HashMap<String, RegistryAuth>> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|mut file| file.read_to_string(&mut text))
        .with_context(|| format!("reading {}", path.display()))?;
    let config: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut auths = HashMap::new();
    for (registry, entry) in config["auths"].as_object().into_iter().flatten() {
        let host = registry
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let (user, password) = if let Some(auth) = entry["auth"].as_str() {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .with_context(|| {
                    format!("{}: auth for {registry} is not base64", path.display())
                })?;
            let (user, password) = decoded
                .split_once(':')
                .with_context(|| format!("{}: auth for {registry}", path.display()))?;
            (user.to_owned(), password.to_owned())
        } else {
            (
                entry["username"].as_str().unwrap_or_default().to_owned(),
                entry["password"].as_str().unwrap_or_default().to_owned(),
            )
        };
        auths.insert(host, RegistryAuth::Basic(user, password));
    }
    Ok(auths)
}

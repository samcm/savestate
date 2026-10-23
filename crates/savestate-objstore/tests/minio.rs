//! Export, download and garbage collection against a real S3-compatible store (CI:
//! MinIO): an interrupted export leaves no readable checkpoint and is resumed
//! without uploading again what it had; a download is the original bit for bit and
//! as sparse; a corrupted chunk is refused; a child checkpoint uploads only its own
//! files; and garbage collection removes exactly what no live checkpoint uses.
//!
//! Needs `SAVESTATE_TEST_S3_ENDPOINT`, `SAVESTATE_TEST_S3_BUCKET` (existing),
//! `SAVESTATE_TEST_S3_REGION`, `SAVESTATE_TEST_S3_ACCESS_KEY_ID` and
//! `SAVESTATE_TEST_S3_SECRET_ACCESS_KEY`. It works under a prefix of its own.

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::{BTreeSet, HashMap},
    os::unix::fs::{FileExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::Duration,
};

use object_store::{ObjectStore as _, ObjectStoreExt as _, PutPayload, aws::AmazonS3Builder};
use rand::RngExt as _;
use savestate_objstore::{CHUNK, Collected, Credentials, Error, Moved, Progress, Remote};
use sha2::{Digest as _, Sha256};

const MIB: u64 = 1 << 20;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an S3-compatible store; see the module docs"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn export_resume_download_and_collect() {
    let prefix = format!("savestate-test-{}-{}", std::process::id(), nanos());
    let remote = remote(&prefix);
    let source = tempfile::tempdir().unwrap();
    let root = source.path();

    // A base image (dense, three chunks), a layer, and a sparse 1 GiB memory file
    // whose data straddles chunk boundaries and ends before a trailing hole.
    write_dense(root, "images/base.qcow2", 2 * CHUNK + 123);
    write_dense(root, "layers/s1-0.qcow2", MIB);
    write_sparse(
        root,
        "checkpoints/c1/memory",
        1 << 30,
        &[
            (0, 4096),
            (100 * MIB, 20 * MIB),
            ((1 << 30) - 2 * 4096, 4096),
        ],
    );
    let c1 = files(&[
        "checkpoints/c1/memory",
        "layers/s1-0.qcow2",
        "images/base.qcow2",
    ]);
    let manifest =
        serde_json::json!({"name": "c1", "chain": ["images/base.qcow2", "layers/s1-0.qcow2"]});

    // Interrupted once its first chunks are in the store.
    let progress = Progress::default();
    tokio::select! {
        result = remote.export("c1", root, &c1, &manifest, &progress) => {
            panic!("the export finished before it was interrupted: {result:?}");
        }
        () = until(|| progress.done() >= CHUNK) => {}
    }
    let total = progress.total();
    assert!(progress.done() < total, "interrupted part way");
    assert_eq!(remote.manifest("c1").await.unwrap(), None);
    assert!(!remote.contains("c1").await.unwrap());

    // Its export record keeps the partial upload from garbage collection.
    let mut cache = HashMap::new();
    let collected = remote.collect(live(&["c1"]), &mut cache).await.unwrap();
    assert_eq!(collected, Collected::default());

    // Resumed: what the interrupted export uploaded is not uploaded again.
    let moved = remote
        .export("c1", root, &c1, &manifest, &Progress::default())
        .await
        .unwrap();
    assert_eq!(moved.copied + moved.present, total);
    assert!(
        moved.present >= CHUNK,
        "the resumed export uploaded everything again: {moved:?}"
    );
    let stored = remote.manifest("c1").await.unwrap().unwrap();
    assert_eq!(stored.checkpoint, manifest);
    assert_eq!(stored.files, c1);

    // Downloaded: identical bytes, the same size and holes, read-only.
    let target = tempfile::tempdir().unwrap();
    let progress = Progress::default();
    let moved = remote
        .download(&targets(target.path(), &c1), &progress)
        .await
        .unwrap();
    assert_eq!(moved.copied, total);
    assert_eq!(progress.done(), total);
    for path in &c1 {
        assert_same(&root.join(path), &target.path().join(path));
    }
    assert!(
        allocated(&target.path().join("checkpoints/c1/memory"))
            <= allocated(&root.join("checkpoints/c1/memory")) + MIB,
        "the downloaded memory file is not sparse"
    );
    assert!(!target.path().join("images/base.qcow2.part").exists());

    // A chunk changed in the store is refused, and nothing is left behind.
    let raw = raw();
    let first = std::fs::read(root.join("images/base.qcow2")).unwrap();
    let chunk = &first[..usize::try_from(CHUNK).unwrap()];
    let key = object_store::path::Path::from(format!(
        "{prefix}/files/images/base.qcow2/{}",
        hex::encode(Sha256::digest(chunk))
    ));
    let mut tampered = chunk.to_vec();
    tampered[4242] ^= 1;
    raw.put(&key, PutPayload::from(tampered)).await.unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let error = remote
        .download(
            &targets(elsewhere.path(), &files(&["images/base.qcow2"])),
            &Progress::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error}");
    assert!(!elsewhere.path().join("images/base.qcow2").exists());
    assert!(!elsewhere.path().join("images/base.qcow2.part").exists());
    raw.put(&key, PutPayload::from(chunk.to_vec()))
        .await
        .unwrap();

    // A child checkpoint shares the image and the parent's layer.
    write_dense(root, "layers/s2-0.qcow2", 3 * MIB);
    write_sparse(
        root,
        "checkpoints/c2/memory",
        1 << 30,
        &[(5 * MIB, 2 * MIB)],
    );
    let c2 = files(&[
        "checkpoints/c2/memory",
        "layers/s2-0.qcow2",
        "layers/s1-0.qcow2",
        "images/base.qcow2",
    ]);
    let moved = remote
        .export(
            "c2",
            root,
            &c2,
            &serde_json::json!({"name": "c2"}),
            &Progress::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        moved,
        Moved {
            copied: 5 * MIB,
            present: 2 * CHUNK + 123 + MIB,
        }
    );

    // c1 is gone: its manifest and memory file go, what c2 shares stays.
    let collected = remote.collect(live(&["c2"]), &mut cache).await.unwrap();
    assert_eq!(
        collected,
        Collected {
            records: 1,
            files: 1,
            stopped: false
        }
    );
    assert!(!remote.contains("c1").await.unwrap());
    let again = tempfile::tempdir().unwrap();
    remote
        .download(&targets(again.path(), &c2), &Progress::default())
        .await
        .unwrap();
    for path in &c2 {
        assert_same(&root.join(path), &again.path().join(path));
    }

    // Nothing is live: the prefix ends up empty.
    let collected = remote.collect(live(&[]), &mut cache).await.unwrap();
    assert_eq!(collected.records, 1);
    let left: Vec<_> = futures::TryStreamExt::try_collect::<Vec<_>>(
        raw.list(Some(&object_store::path::Path::from(prefix.as_str()))),
    )
    .await
    .unwrap();
    assert!(left.is_empty(), "left in the store: {left:?}");
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn credentials() -> Credentials {
    Credentials {
        access_key_id: env("SAVESTATE_TEST_S3_ACCESS_KEY_ID"),
        secret_access_key: env("SAVESTATE_TEST_S3_SECRET_ACCESS_KEY"),
        session_token: None,
    }
}

fn remote(prefix: &str) -> Remote {
    Remote::s3(
        &env("SAVESTATE_TEST_S3_ENDPOINT"),
        &env("SAVESTATE_TEST_S3_BUCKET"),
        &env("SAVESTATE_TEST_S3_REGION"),
        prefix,
        &credentials(),
    )
    .unwrap()
}

/// The same bucket without savestate in between, to look at and change objects.
fn raw() -> impl object_store::ObjectStore {
    let endpoint = env("SAVESTATE_TEST_S3_ENDPOINT");
    let credentials = credentials();
    AmazonS3Builder::new()
        .with_endpoint(&endpoint)
        .with_bucket_name(env("SAVESTATE_TEST_S3_BUCKET"))
        .with_region(env("SAVESTATE_TEST_S3_REGION"))
        .with_access_key_id(credentials.access_key_id)
        .with_secret_access_key(credentials.secret_access_key)
        .with_allow_http(endpoint.starts_with("http://"))
        .build()
        .unwrap()
}

/// The ids `collect` is told are live.
fn live(
    ids: &[&str],
) -> impl FnOnce() -> std::future::Ready<
    Result<BTreeSet<String>, Box<dyn std::error::Error + Send + Sync>>,
> {
    let ids: BTreeSet<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    move || std::future::ready(Ok(ids))
}

fn files(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

fn targets(root: &Path, paths: &[String]) -> Vec<(String, PathBuf)> {
    paths
        .iter()
        .map(|path| {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            (path.clone(), target)
        })
        .collect()
}

fn write_dense(root: &Path, path: &str, len: u64) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    let mut data = vec![0u8; usize::try_from(len).unwrap()];
    rand::rng().fill(&mut data[..]);
    std::fs::write(full, data).unwrap();
}

fn write_sparse(root: &Path, path: &str, size: u64, extents: &[(u64, u64)]) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&full).unwrap();
    file.set_len(size).unwrap();
    for &(offset, len) in extents {
        let mut data = vec![0u8; usize::try_from(len).unwrap()];
        rand::rng().fill(&mut data[..]);
        // No zero bytes at the edges, so the extent is exactly this long.
        data[0] = 1;
        *data.last_mut().unwrap() = 1;
        file.write_all_at(&data, offset).unwrap();
    }
    file.sync_all().unwrap();
}

fn assert_same(original: &Path, copy: &Path) {
    let (a, b) = (
        std::fs::read(original).unwrap(),
        std::fs::read(copy).unwrap(),
    );
    assert!(
        a == b,
        "{} differs from {}",
        copy.display(),
        original.display()
    );
    let mode = std::fs::metadata(copy).unwrap().permissions().mode();
    assert_eq!(mode & 0o222, 0, "{} is writable", copy.display());
}

fn allocated(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks() * 512
}

async fn until(done: impl Fn() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

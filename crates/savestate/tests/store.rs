//! A node's state directory after crashes at each publication boundary of a
//! checkpoint, and garbage collection over layer chains. Plain files stand in for
//! qcow2 layers and memory files: the store never reads them.

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use orbd_qemu::qmp::FileMigration;
use orbd_vm::fsutil::write_json_atomic;
use savestate::store::{Live, Manifest, Store, image_path, layer_path};
use savestate_api::Fingerprint;

fn manifest(name: &str, chain: &[String]) -> Manifest {
    Manifest {
        namespace: "sandboxes".into(),
        name: name.into(),
        sandbox: "web-1".into(),
        parent: None,
        vcpus: 1,
        memory_mib: 1024,
        disk_bytes: 4 << 30,
        guest_time_unix_ns: None,
        chain: chain.to_vec(),
        machine_args: vec!["-m".into(), "1024M".into()],
        migration: FileMigration {
            multifd_channels: 8,
            direct_io: false,
        },
        memory_bytes: 1,
        layer_bytes: 1,
        fingerprint: Fingerprint {
            launcher_image: "ghcr.io/example/savestate@sha256:0".into(),
            qemu: "QEMU emulator version 11.1.2".into(),
            machine_type: "pc-q35-11.1".into(),
            cpu_model: "EPYC".into(),
            host_cpu: None,
            devices: "sha256:0".into(),
        },
    }
}

fn touch(root: &Path, relative: &str) {
    std::fs::write(root.join(relative), b"x").unwrap();
}

/// A save as the launcher leaves it in `checkpoints/<id>.tmp/`: the memory file,
/// then (if `complete`) the manifest.
fn save(store: &Store, id: &str, chain: &[String], complete: bool) {
    let tmp = store.checkpoints().fresh_tmp(id).unwrap();
    std::fs::write(tmp.join("memory"), b"ram").unwrap();
    if complete {
        write_json_atomic(&tmp.join("manifest.json"), &manifest(id, chain)).unwrap();
    }
}

#[test]
fn a_crash_at_each_publication_boundary_converges() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_owned());
    for sub in ["images", "layers", "checkpoints", "trash"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    let chain = vec![image_path("aa"), layer_path("s1-0")];

    // Crashed while QEMU wrote the memory file: nothing is published.
    save(&store, "interrupted", &chain, false);
    assert_eq!(store.settle("interrupted").unwrap(), None);
    assert!(!store.checkpoints().tmp("interrupted").exists());
    assert!(!store.checkpoints().published("interrupted").exists());

    // Crashed after the manifest, before the rename: published now, intact.
    save(&store, "written", &chain, true);
    let settled = store.settle("written").unwrap().unwrap();
    assert_eq!(settled.chain, chain);
    assert!(!store.checkpoints().tmp("written").exists());
    assert_eq!(
        std::fs::read(store.checkpoints().published("written").join("memory")).unwrap(),
        b"ram"
    );

    // Crashed after the rename, before the status was written: the same answer, and
    // settling again changes nothing.
    assert_eq!(store.settle("written").unwrap().unwrap(), settled);
    assert_eq!(store.manifest("written").unwrap(), settled);

    // Never started here: nothing.
    assert_eq!(store.settle("elsewhere").unwrap(), None);
}

#[test]
fn garbage_collection_keeps_what_live_objects_build_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::new(root.to_owned());
    for sub in ["images", "layers", "checkpoints", "trash"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    // Image `base` booted sandbox s1, checkpointed as `kept` (its layer s1-0). A
    // sandbox s2 restored from `kept` was checkpointed as `gone` (layer s2-0), then s2
    // and `gone` were deleted. Sandbox s3 runs from image `other`; image `unused` and
    // s4's layer belong to nothing left; so does image `recent`, but a sandbox booted
    // from it moments ago. s5 is gone, but a complete save whose object still exists
    // uses its layer.
    for image in ["base", "other", "unused", "recent"] {
        touch(root, &image_path(image));
    }
    let long_ago = std::time::SystemTime::now() - 2 * savestate::store::IMAGE_GRACE;
    std::fs::File::open(root.join(image_path("unused")))
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    for layer in [
        "s1-0", "s1-1", "s2-0", "s2-1", "s3-0", "s4-0", "s5-0", "s6-0",
    ] {
        touch(root, &layer_path(layer));
    }
    let kept = vec![image_path("base"), layer_path("s1-0")];
    let gone = vec![image_path("base"), layer_path("s1-0"), layer_path("s2-0")];
    let saving = vec![image_path("base"), layer_path("s1-0"), layer_path("s5-0")];
    save(&store, "kept", &kept, true);
    store.checkpoints().publish("kept").unwrap();
    save(&store, "gone", &gone, true);
    store.checkpoints().publish("gone").unwrap();
    save(&store, "saving", &saving, true);
    save(&store, "abandoned", &kept, false);

    let inventory = store.inventory();
    let live = Live {
        sandboxes: ["s1", "s3"].map(String::from).into(),
        checkpoints: ["kept", "saving"].map(String::from).into(),
        images: ["other"].map(String::from).into(),
        // An import is writing s6's layer; its checkpoint is not published yet.
        files: [layer_path("s6-0")].into(),
    };
    store.collect(&inventory, &live).unwrap();

    let exists = |relative: &str| root.join(relative).exists();
    // Checkpoints and saves whose objects are gone.
    assert!(store.checkpoints().published("kept").exists());
    assert!(!store.checkpoints().published("gone").exists());
    assert!(store.checkpoints().tmp("saving").exists());
    assert!(!store.checkpoints().tmp("abandoned").exists());
    // Layers: a live sandbox's, those in remaining chains (also of a save that is
    // complete but not yet published), and one an import is writing.
    for layer in ["s1-0", "s1-1", "s3-0", "s5-0", "s6-0"] {
        assert!(exists(&layer_path(layer)), "{layer} was removed");
    }
    for layer in ["s2-0", "s2-1", "s4-0"] {
        assert!(!exists(&layer_path(layer)), "{layer} was kept");
    }
    // Images: in a remaining chain, booted by a live sandbox, or used lately.
    assert!(exists(&image_path("base")));
    assert!(exists(&image_path("other")));
    assert!(!exists(&image_path("unused")));
    // Kept a while after its last use, so creating a sandbox again pulls nothing.
    assert!(exists(&image_path("recent")));
}

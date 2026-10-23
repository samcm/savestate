//! savestate end to end on a real cluster with KVM: the CLI creates a sandbox from a
//! base image, state is written into it, it is checkpointed while it keeps running,
//! two sandboxes start from the checkpoint (one with a frozen clock, one stepped),
//! and each is shown to have resumed exactly the checkpoint's state, independently of
//! the other and of the original, which carries on unaffected. A sandbox forked from
//! a restored one carries both histories. Along the way: labels and selectors,
//! waiting for a sandbox that fails, exec with environment and working directory,
//! detached processes that outlive a guest-agent restart, and file and directory
//! copies. A checkpoint exported to an object store outlives its node-local copy: a
//! sandbox started from it once the node has dropped its copy imports it from the
//! store and resumes exactly its state. A guest port is unreachable until the
//! sandbox exposes it; then it answers through the sandbox's Service to clients the
//! installation allows and to no one else, and a fork of the sandbox answers through
//! its own Service. Port-forward reaches any guest port.
//!
//! Needs a disposable cluster in `KUBECONFIG` with savestate installed and at least
//! three 1-vCPU VMs' room on one node (CI: kind on a KVM runner), and:
//!
//! - `SAVESTATE_E2E_IMAGE`: the base image (`…@sha256:…`) holding `/rootfs.qcow2`
//! - `SAVESTATE_NAMESPACE`: a sandbox namespace from the installation config
//! - `SAVESTATE_E2E_ALLOWED`: `IP:PORT` the egress allowlist lets guests reach
//! - `SAVESTATE_E2E_BLOCKED`: `IP:PORT` in a private range guests must not reach
//! - `SAVESTATE_E2E_NODE`: the VM node, a Docker container (kind) whose state
//!   directory the test looks into
//! - `SAVESTATE_E2E_STORE`: a store from the installation config, whose node-local
//!   copies the node agent drops as soon as they are unused (`cache.minFreePercent:
//!   100`), and `SAVESTATE_E2E_S3_ENDPOINT`, `_BUCKET`, `_PREFIX`, `_ACCESS_KEY_ID`
//!   and `_SECRET_ACCESS_KEY` to look into it
//! - `SAVESTATE_E2E_CLIENTS`: the namespace `network.exposeFrom` allows, and
//!   `SAVESTATE_E2E_CLIENT_IMAGE`, an image with bash and coreutils for client pods
//!   there and in `default` (which it does not allow); `kubectl` on the `PATH`

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs::Permissions,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    process::{Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use k8s_openapi::api::{
    core::v1::{Node, Pod, Service},
    events::v1::Event,
};
use kube::{
    Api, Client, ResourceExt as _,
    api::{AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams},
};
use savestate_api::{Checkpoint, SandboxPhase, Transfer, TransferPhase};

#[tokio::test]
#[ignore = "needs a cluster with savestate installed on KVM nodes; see the module docs"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn checkpoint_fork_and_resume_exactly() {
    let image = env("SAVESTATE_E2E_IMAGE");
    let ns = env("SAVESTATE_NAMESPACE");
    let allowed = env("SAVESTATE_E2E_ALLOWED");
    let blocked = env("SAVESTATE_E2E_BLOCKED");
    let client = Client::try_default().await.expect("KUBECONFIG");
    let checkpoints: Api<Checkpoint> = Api::namespaced(client.clone(), &ns);

    // A sandbox from the base image.
    let started = Instant::now();
    savestate(&[
        "create",
        "web",
        "--image",
        &image,
        "--vcpus",
        "1",
        "--memory",
        "1Gi",
        "--disk",
        "4Gi",
        "--label",
        "app=web",
        "--label",
        "tier=test",
    ]);
    report(&format!(
        "boot: create to Running in {:.1} s",
        secs(started)
    ));
    assert_eq!(sh("web", "cat /proc/sys/kernel/hostname").trim(), "web");
    let pod = Api::<Pod>::namespaced(client.clone(), &ns)
        .get("web")
        .await
        .unwrap();
    let container = &pod.spec.unwrap().containers[0];
    let security = serde_json::to_value(&container.security_context).unwrap();
    report(&format!("VM pod security context: {security}"));
    // Not privileged: devices come from the node agent's device plugin, and the
    // runtime confines the container with savestate's seccomp profile.
    assert_eq!(security.get("privileged"), None, "{security}");
    assert_eq!(
        security["seccompProfile"],
        serde_json::json!({"type": "Localhost", "localhostProfile": "savestate/vm.json"})
    );
    let init = sh_pod(
        &client,
        &ns,
        "web",
        "grep -E '^Seccomp(_filters)?:' /proc/1/status",
    )
    .await;
    assert!(
        init.contains("Seccomp:\t2"),
        "the container runs without a seccomp filter: {init}"
    );
    // QEMU runs unprivileged, as the QEMU user.
    let qemu = sh_pod(
        &client,
        &ns,
        "web",
        "for p in /proc/[0-9]*; do [ \"$(cat $p/comm)\" = qemu-system-x86 ] || continue; \
         awk '/^(Uid|CapEff):/' $p/status; tr '\\0' ' ' <$p/cmdline; done",
    )
    .await;
    report(&format!("QEMU process: {qemu}"));
    assert!(
        qemu.contains("Uid:\t65532\t65532\t65532\t65532"),
        "QEMU's user: {qemu}"
    );
    assert!(
        qemu.contains("CapEff:\t0000000000000000"),
        "QEMU's capabilities: {qemu}"
    );
    assert!(qemu.contains("-sandbox on"), "{qemu}");

    // A shape that requests less CPU than its vCPUs (setup.sh: 4 vCPUs, cpuRequest
    // 500m): the pod is Burstable with no CPU limit, memory is still request = limit,
    // and the guest has all four vCPUs. On a 4-CPU node that also runs the cluster,
    // four whole CPUs would not fit next to `web`.
    savestate(&[
        "create", "burst", "--image", &image, "--vcpus", "4", "--memory", "1Gi", "--disk", "4Gi",
    ]);
    let pod = Api::<Pod>::namespaced(client.clone(), &ns)
        .get("burst")
        .await
        .unwrap();
    let resources =
        serde_json::to_value(&pod.spec.as_ref().unwrap().containers[0].resources).unwrap();
    report(&format!(
        "VM pod resources (4 vCPUs, cpuRequest 500m): {resources}"
    ));
    assert_eq!(resources["requests"]["cpu"], "500m", "{resources}");
    assert_eq!(resources["limits"].get("cpu"), None, "{resources}");
    assert_eq!(
        resources["limits"]["memory"], resources["requests"]["memory"],
        "{resources}"
    );
    assert_eq!(pod.status.unwrap().qos_class.as_deref(), Some("Burstable"));
    assert_eq!(sh("burst", "nproc").trim(), "4");
    let node = Api::<Node>::all(client.clone())
        .get(pod.spec.unwrap().node_name.as_deref().unwrap())
        .await
        .unwrap();
    let node_name = node.metadata.name.clone().unwrap();
    let allocatable = &node.status.unwrap().allocatable.unwrap()["cpu"].0;
    report(&format!("node allocatable CPU: {allocatable}"));
    savestate(&["delete", "sandbox", "burst"]);

    // A sandbox that fails (a shape the installation does not offer): create waits
    // for it and exits non-zero with its message.
    let failed = cli()
        .args([
            "create",
            "unoffered",
            "--image",
            &image,
            "--vcpus",
            "2",
            "--memory",
            "1Gi",
            "--disk",
            "4Gi",
            "--timeout",
            "2m",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "{}", describe(&failed));
    assert!(
        stderr.contains("sandbox unoffered failed") && stderr.contains("not offered"),
        "{stderr}"
    );
    savestate(&["delete", "sandbox", "unoffered"]);

    // Egress: the allowlisted address is reachable, other private ones are not.
    let (ip, port) = allowed.split_once(':').unwrap();
    sh(
        "web",
        &format!("timeout 10 bash -c 'exec 3<>/dev/tcp/{ip}/{port}'"),
    );
    let (ip, port) = blocked.split_once(':').unwrap();
    let refused = exec(
        "web",
        &format!("timeout 5 bash -c 'exec 3<>/dev/tcp/{ip}/{port}'"),
    );
    assert!(!refused.status.success(), "the guest reached {blocked}");

    // exec passes exit codes; cp copies files both ways.
    assert_eq!(exec("web", "exit 3").status.code(), Some(3));
    let dir = tempfile::tempdir().unwrap();
    let upload = dir.path().join("upload");
    std::fs::write(&upload, b"copied in\n").unwrap();
    savestate(&["cp", upload.to_str().unwrap(), "web:/root/copied"]);
    assert_eq!(sh("web", "cat /root/copied"), "copied in\n");
    let download = dir.path().join("download");
    savestate(&["cp", "web:/etc/hostname", download.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&download).unwrap(), "web\n");

    // exec with environment variables and a working directory.
    let out = cli()
        .args([
            "exec",
            "web",
            "--env",
            "A=1",
            "--env",
            "B=two words",
            "--cwd",
            "/tmp",
        ])
        .args(["--", "sh", "-c", "echo \"$A|$B|$(pwd)\""])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "1|two words|/tmp\n");

    // cp creates missing directories, and sets --mode; directories copy both ways.
    savestate(&[
        "cp",
        upload.to_str().unwrap(),
        "web:/root/new/deeper/copied",
        "--mode",
        "600",
    ]);
    assert_eq!(
        sh(
            "web",
            "stat -c '%a %n' /root/new /root/new/deeper /root/new/deeper/copied; cat /root/new/deeper/copied"
        ),
        "755 /root/new\n755 /root/new/deeper\n600 /root/new/deeper/copied\ncopied in\n"
    );
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub/run"), "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(tree.join("sub/run"), Permissions::from_mode(0o750)).unwrap();
    std::fs::write(tree.join("top"), "top\n").unwrap();
    savestate(&["cp", tree.to_str().unwrap(), "web:/srv/tree"]);
    assert_eq!(
        sh(
            "web",
            "/srv/tree/sub/run; stat -c %a /srv/tree/sub/run; cat /srv/tree/top"
        ),
        "ran\n750\ntop\n"
    );
    let back = dir.path().join("back");
    savestate(&["cp", "web:/srv/tree", back.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(back.join("top")).unwrap(), "top\n");
    let run = std::fs::metadata(back.join("sub/run")).unwrap();
    assert_eq!(run.permissions().mode() & 0o7777, 0o750);

    // A detached process outlives its exec and a restart of the guest agent, and its
    // status and output stay readable.
    let id = savestate(&[
        "exec", "web", "--detach", "--env", "WHO=detached", "--cwd", "/root", "--", "sh", "-c",
        "echo \"$WHO in $(pwd)\"; echo warning >&2; while [ ! -e /run/go ]; do sleep 0.1; done; echo done; exit 7",
    ])
    .trim()
    .to_owned();
    let main_pid = format!("systemctl show -P MainPID savestate-exec-{id}");
    let pid_before = sh("web", &main_pid);
    let agent = "systemctl show -P MainPID savestate-guest";
    let agent_before = sh("web", agent);
    // Detached itself: an attached exec would end with the agent it restarts.
    savestate(&[
        "exec",
        "web",
        "--detach",
        "--",
        "systemctl",
        "restart",
        "savestate-guest",
    ]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = exec("web", agent);
        let now = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() && now != agent_before && now.trim() != "0" {
            break;
        }
        assert!(Instant::now() < deadline, "the guest agent did not restart");
        std::thread::sleep(Duration::from_millis(500));
    }
    let ps = json(&["ps", "web", &id, "-o", "json"]);
    assert_eq!(ps[0]["state"], "running", "{ps}");
    assert_eq!(sh("web", &main_pid), pid_before, "the same process runs");
    sh("web", "touch /run/go");
    let deadline = Instant::now() + Duration::from_secs(30);
    let ps = loop {
        let ps = json(&["ps", "web", &id, "-o", "json"]);
        if ps[0]["state"] == "exited" {
            break ps;
        }
        assert!(Instant::now() < deadline, "{ps}");
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(ps[0]["exitCode"], 7, "{ps}");
    let logs = cli().args(["logs", "web", &id]).output().unwrap();
    assert!(logs.status.success(), "{}", describe(&logs));
    assert_eq!(
        (logs.stdout.as_slice(), logs.stderr.as_slice()),
        (&b"detached in /root\ndone\n"[..], &b"warning\n"[..])
    );
    let follow = cli()
        .args(["logs", "web", &id, "--follow"])
        .output()
        .unwrap();
    assert_eq!(follow.status.code(), Some(7), "{}", describe(&follow));
    assert_eq!(follow.stdout, logs.stdout);
    savestate(&["ps", "web"]);

    // State: a process counting in its memory, and a file on disk.
    sh(
        "web",
        "setsid sh -c 'i=0; while :; do i=$((i+1)); echo \"$$ $i\" >/run/counter.new && mv /run/counter.new /run/counter; sleep 0.1; done' </dev/null >/dev/null 2>&1 &",
    );
    sh("web", "echo before >/root/state && sync");
    // Page cache, which the pre-checkpoint hook drops: the memory file holds about
    // the memory the guest uses, not its cache.
    sh(
        "web",
        "head -c 256M /dev/urandom >/root/cache && sync && cat /root/cache >/dev/null",
    );
    let used = guest_used_bytes("web");
    std::thread::sleep(Duration::from_secs(2));
    let (pid, at_checkpoint) = counter("web");
    let machine_id = sh("web", "cat /etc/machine-id");

    // Checkpoint while it runs, then change the original.
    let started = Instant::now();
    savestate(&[
        "checkpoint",
        "web",
        "--name",
        "web-1",
        "--label",
        "tier=parked",
    ]);
    report(&format!(
        "checkpoint: create to Ready in {:.1} s; {}",
        secs(started),
        saved_note(&client, &ns, "web-1").await
    ));
    let ready = checkpoints.get("web-1").await.unwrap().status.unwrap();
    let memory_bytes = ready.memory_bytes.unwrap();
    report(&format!(
        "checkpoint memory file: {:.0} MiB; the guest used {:.0} MiB besides 256 MiB of page cache",
        f64_mib(memory_bytes),
        f64_mib(used)
    ));
    assert!(
        memory_bytes <= used + (96 << 20),
        "the memory file holds {memory_bytes} bytes; the guest used {used} besides its page cache"
    );
    let guest_time = ready.guest_time.unwrap().0.as_second();
    assert_eq!(ready.vcpus, Some(1));
    assert_eq!(ready.memory.unwrap().0, "1Gi");
    assert_eq!(ready.parent, None);
    // The sandbox's labels, overridden where the checkpoint says otherwise.
    assert_eq!(
        labels(&json(&["get", "web-1", "-o", "json"])),
        [("app", "web"), ("tier", "parked")]
    );
    let (same_pid, after) = counter("web");
    assert_eq!(
        same_pid, pid,
        "the original's process survived the checkpoint"
    );
    assert!(
        after > at_checkpoint,
        "the original carries on after the checkpoint"
    );
    sh("web", "echo after >/root/state && sync");
    // Let host time pass, so a frozen clock shows.
    std::thread::sleep(Duration::from_secs(20));

    // Two sandboxes from the checkpoint at once: frozen and stepped clocks.
    let started = Instant::now();
    let forks: Vec<_> = [("web-frozen", "frozen"), ("web-stepped", "stepped")]
        .into_iter()
        .map(|(name, clock)| {
            let child = cli()
                .args(["create", name, "--from", "web-1", "--clock", clock])
                .args(["--label", &format!("clock={clock}")])
                .spawn()
                .unwrap();
            (name, child)
        })
        .collect();
    for (name, child) in forks {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "create {name}");
        report(&format!(
            "restore: {name} create to Running in {:.1} s (two concurrent restores)",
            secs(started)
        ));
    }
    let resumed_at = unix_now();

    for fork in ["web-frozen", "web-stepped"] {
        // The same process, resumed from the checkpoint's count, still counting.
        let (fork_pid, count) = counter(fork);
        assert_eq!(fork_pid, pid, "{fork}: the counting process was resumed");
        assert!(
            count >= at_checkpoint,
            "{fork}: count {count} < {at_checkpoint} at the checkpoint"
        );
        std::thread::sleep(Duration::from_secs(1));
        assert!(counter(fork).1 > count, "{fork}: the process runs");
        let (_, original) = counter("web");
        assert!(
            count < original,
            "{fork}: count {count} is not behind the original's {original}: it did not resume the checkpoint"
        );
        // The disk as of the checkpoint, and its own identity.
        assert_eq!(sh(fork, "cat /root/state"), "before\n", "{fork}");
        assert_eq!(sh(fork, "cat /proc/sys/kernel/hostname").trim(), fork);
        assert_ne!(sh(fork, "cat /etc/machine-id"), machine_id, "{fork}");
    }

    // Clocks: frozen resumes at the checkpoint's guest time; stepped at the host's.
    let host_gap = resumed_at - guest_time;
    assert!(
        host_gap >= 20,
        "the restores came {host_gap} s after the checkpoint"
    );
    let frozen: i64 = sh("web-frozen", "date +%s").trim().parse().unwrap();
    let elapsed = unix_now() - resumed_at;
    assert!(
        frozen - guest_time <= elapsed + 10,
        "frozen clock at {frozen}: {} s after the checkpoint's guest time, though the guest ran for about {elapsed} s",
        frozen - guest_time
    );
    let stepped: i64 = sh("web-stepped", "date +%s").trim().parse().unwrap();
    assert!(
        (stepped - unix_now()).abs() <= 5,
        "stepped clock at {stepped}, host at {}",
        unix_now()
    );
    report(&format!(
        "clocks: {host_gap} s after the checkpoint, frozen guest {} s behind the host, stepped {} s",
        unix_now() - frozen,
        unix_now() - stepped
    ));

    // Labels: sandboxes from a checkpoint carry its labels; list selects by them.
    assert_eq!(
        labels(&json(&["get", "web-frozen", "-o", "json"])),
        [("app", "web"), ("clock", "frozen"), ("tier", "parked")]
    );
    assert_eq!(
        names(&json(&["list", "sandboxes", "-l", "app=web", "-o", "json"])),
        ["web", "web-frozen", "web-stepped"]
    );
    assert_eq!(
        names(&json(&[
            "list",
            "sandboxes",
            "-l",
            "tier=parked,clock!=frozen",
            "-o",
            "json"
        ])),
        ["web-stepped"]
    );
    assert_eq!(
        names(&json(&[
            "list",
            "checkpoints",
            "-l",
            "app=web",
            "-o",
            "json"
        ])),
        ["web-1"]
    );
    let table = savestate(&["list", "sandboxes", "-l", "clock=frozen"]);
    let rows: Vec<Vec<&str>> = table
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(rows[0], ["NAME", "PHASE", "NODE", "FROM", "AGE"], "{table}");
    assert_eq!(rows.len(), 2, "{table}");
    assert_eq!(
        rows[1][..4],
        ["web-frozen", "Running", node_name.as_str(), "web-1"]
    );
    let shown = savestate(&["get", "web-frozen"]);
    assert!(shown.contains("1 vCPU, 1Gi memory, 4Gi disk"), "{shown}");

    // Independent: a write in one fork is seen nowhere else; the original keeps its own.
    sh("web-frozen", "echo frozen >/root/mine && sync");
    for other in ["web-stepped", "web"] {
        assert_eq!(
            exec(other, "test -e /root/mine").status.code(),
            Some(1),
            "{other}"
        );
    }
    assert_eq!(sh("web", "cat /root/state"), "after\n");
    let (original_pid, _) = counter("web");
    assert_eq!(original_pid, pid);

    for pod in ["web", "web-frozen"] {
        report(&format!(
            "memory: VM pod {pod} cgroup peak {} MiB (1 GiB guest, 1.5 GiB limit)",
            peak_mib(&client, &ns, pod).await
        ));
    }

    // Fork a restored sandbox: its checkpoint descends from web-1 and the new sandbox
    // carries both the original process and the fork's own write.
    savestate(&["delete", "sandbox", "web-stepped"]);
    let forked = json(&[
        "fork",
        "web-frozen",
        "-n",
        "1",
        "--label",
        "generation=2",
        "-o",
        "json",
    ]);
    let grandchild = names(&forked["sandboxes"]).remove(0);
    let checkpoint = forked["checkpoint"]["metadata"]["name"].as_str().unwrap();
    assert_eq!(forked["sandboxes"][0]["status"]["phase"], "Running");
    let status = checkpoints.get(checkpoint).await.unwrap().status.unwrap();
    assert_eq!(status.parent.as_deref(), Some("web-1"));
    // The checkpoint has its sandbox's labels; the new sandbox those and its own.
    assert_eq!(
        labels(&forked["checkpoint"]),
        [("app", "web"), ("clock", "frozen"), ("tier", "parked")]
    );
    assert_eq!(
        labels(&forked["sandboxes"][0]),
        [
            ("app", "web"),
            ("clock", "frozen"),
            ("generation", "2"),
            ("tier", "parked")
        ]
    );
    assert_eq!(
        sh(&grandchild, "cat /root/mine /root/state"),
        "frozen\nbefore\n"
    );
    assert_eq!(counter(&grandchild).0, pid);
    report(&format!(
        "fork of a restored sandbox: {}",
        saved_note(&client, &ns, checkpoint).await
    ));

    for sandbox in ["web", "web-frozen", grandchild.as_str()] {
        savestate(&["delete", "sandbox", sandbox]);
    }
    for checkpoint in ["web-1", checkpoint] {
        savestate(&["delete", "checkpoint", checkpoint]);
    }
}

#[tokio::test]
#[ignore = "needs a cluster with savestate installed on KVM nodes; see the module docs"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn export_drop_the_local_copy_and_restore_from_the_store() {
    let image = env("SAVESTATE_E2E_IMAGE");
    let ns = env("SAVESTATE_NAMESPACE");
    let node = env("SAVESTATE_E2E_NODE");
    let store = env("SAVESTATE_E2E_STORE");
    let client = Client::try_default().await.expect("KUBECONFIG");
    let checkpoints: Api<Checkpoint> = Api::namespaced(client.clone(), &ns);
    let transfers: Api<Transfer> = Api::namespaced(client.clone(), &ns);

    savestate(&[
        "create", "parked", "--image", &image, "--vcpus", "1", "--memory", "1Gi", "--disk", "4Gi",
    ]);
    sh(
        "parked",
        "setsid sh -c 'i=0; while :; do i=$((i+1)); echo \"$$ $i\" >/run/counter.new && mv /run/counter.new /run/counter; sleep 0.1; done' </dev/null >/dev/null 2>&1 &",
    );
    sh("parked", "echo kept >/root/state && sync");
    std::thread::sleep(Duration::from_secs(2));

    // Checkpointed but in no store: its node is kept from scale-down.
    let (pid, at_checkpoint) = counter("parked");
    savestate(&["checkpoint", "parked", "--name", "parked-1"]);
    sh("parked", "echo later >/root/state && sync");
    let uid = checkpoints.get("parked-1").await.unwrap().uid().unwrap();
    assert_eq!(
        scale_down(&client, &node).await.as_deref(),
        Some("true"),
        "a node holding the only copy of a checkpoint may be scaled down"
    );

    // Exported (asked for after the fact): the node may be scaled down again.
    let started = Instant::now();
    checkpoints
        .patch(
            "parked-1",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"spec": {"export": store}})),
        )
        .await
        .unwrap();
    wait_for("the export", 600, || async {
        let status = checkpoints.get("parked-1").await.unwrap().status.unwrap();
        status.stores.contains(&store)
    })
    .await;
    let export = transfers
        .get(&format!("parked-1-to-{store}"))
        .await
        .unwrap();
    assert_eq!(export.spec.from.node.as_deref(), Some(node.as_str()));
    report(&format!(
        "export: spec.export set to Ready in {store} in {:.1} s; {}",
        secs(started),
        note(&client, &ns, &export.name_any(), "Copied").await
    ));
    wait_for("scale-down to be allowed again", 120, || async {
        scale_down(&client, &node).await.is_none()
    })
    .await;
    report(&format!("store objects: {}", store_objects(&uid).await));

    // Parked: the sandbox goes, and the node drops its copy; only the store has it.
    savestate(&["delete", "sandbox", "parked"]);
    wait_for("the node to drop its copy", 300, || async {
        let status = checkpoints.get("parked-1").await.unwrap().status.unwrap();
        status.nodes.is_empty()
    })
    .await;
    let local = format!("/var/lib/savestate/checkpoints/{uid}");
    let gone = Command::new("docker")
        .args(["exec", &node, "test", "!", "-e", &local])
        .status()
        .unwrap();
    assert!(gone.success(), "{local} is still on {node}");
    report(&format!(
        "evicted: {}",
        note(&client, &ns, "parked-1", "Evicted").await
    ));

    // Restored from the store: imported to the node, then resumed exactly.
    let started = Instant::now();
    savestate(&["create", "parked-back", "--from", "parked-1"]);
    let import = transfers.get("parked-back").await.unwrap();
    assert_eq!(import.spec.from.store.as_deref(), Some(store.as_str()));
    assert_eq!(import.spec.to.node.as_deref(), Some(node.as_str()));
    assert_eq!(import.status.unwrap().phase, TransferPhase::Ready);
    report(&format!(
        "restore from the store: create to Running in {:.1} s; {}; sandbox was {}",
        secs(started),
        note(&client, &ns, "parked-back", "Copied").await,
        note(
            &client,
            &ns,
            "parked-back",
            &format!("{:?}", SandboxPhase::WaitingForTransfer)
        )
        .await,
    ));
    let (restored_pid, count) = counter("parked-back");
    assert_eq!(restored_pid, pid, "the counting process was resumed");
    assert!(
        count >= at_checkpoint,
        "count {count} is behind {at_checkpoint}, read before the checkpoint"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert!(counter("parked-back").1 > count, "the process runs");
    assert_eq!(sh("parked-back", "cat /root/state"), "kept\n");
    let status = checkpoints.get("parked-1").await.unwrap().status.unwrap();
    assert_eq!(
        status.nodes,
        std::slice::from_ref(&node),
        "the import is a node-local copy again"
    );

    savestate(&["delete", "sandbox", "parked-back"]);
    savestate(&["delete", "checkpoint", "parked-1"]);
}

/// The node's scale-down annotation.
#[tokio::test]
#[ignore = "needs a cluster with savestate installed on KVM nodes; see the module docs"]
#[expect(clippy::too_many_lines, reason = "one scenario, read top to bottom")]
async fn exposed_ports_reach_allowed_clients_and_each_fork() {
    let image = env("SAVESTATE_E2E_IMAGE");
    let ns = env("SAVESTATE_NAMESPACE");
    let clients = env("SAVESTATE_E2E_CLIENTS");
    let client_image = env("SAVESTATE_E2E_CLIENT_IMAGE");
    let client = Client::try_default().await.expect("KUBECONFIG");
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);

    // A pod in the namespace `network.exposeFrom` allows, and one elsewhere.
    let inside = (clients.as_str(), "inside");
    let outside = ("default", "outside");
    for (namespace, name) in [inside, outside] {
        client_pod(&client, namespace, name, &client_image).await;
    }

    // A sandbox serving HTTP on 8080: each answer is its hostname, so it says which
    // sandbox answered.
    savestate(&[
        "create", "site", "--image", &image, "--vcpus", "1", "--memory", "1Gi", "--disk", "4Gi",
    ]);
    sh(
        "site",
        r#"cat >/usr/local/bin/hello <<'END'
#!/bin/bash
while IFS= read -r line && [ -n "${line%$'\r'}" ]; do :; done
printf 'HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\n%s\n' "$(cat /proc/sys/kernel/hostname)"
END
chmod +x /usr/local/bin/hello
systemd-run --unit=hello systemd-socket-activate --listen=8080 --accept --inetd /usr/local/bin/hello"#,
    );
    let site_ip = pod_ip(&pods, "site").await;

    // Not exposed: nothing reaches it, not even from the allowed namespace.
    assert_eq!(
        fetch(&client, inside, &format!("{site_ip}:8080")).await,
        None,
        "an unexposed port answered"
    );

    // Port-forward reaches any guest port through the Kubernetes API, exposed or not.
    let mut forward = cli()
        .args(["port-forward", "site", "18080:8080"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut answer = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while answer.is_none() && Instant::now() < deadline {
        answer = fetch_local("127.0.0.1:18080");
        if answer.is_none() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    forward.kill().unwrap();
    forward.wait().unwrap();
    assert_eq!(answer.as_deref(), Some("site"), "port-forward");

    // Exposed with a Service: allowed clients reach it, others do not.
    let started = Instant::now();
    let out = savestate(&["expose", "site", "8080", "--service"]);
    assert!(out.contains("site exposes 8080/tcp"), "{out}");
    let site_service = service_ip(&services, "site").await;
    let site_url = format!("{site_service}:8080");
    wait_for("the exposed port to answer", 120, || async {
        fetch(&client, inside, &site_url).await.as_deref() == Some("site")
    })
    .await;
    report(&format!(
        "expose: the Service answered {:.1} s after `savestate expose`",
        secs(started)
    ));
    assert_eq!(
        fetch(&client, outside, &site_url).await,
        None,
        "a client outside network.exposeFrom reached the exposed port"
    );
    assert_eq!(
        fetch(&client, outside, &format!("{site_ip}:8080")).await,
        None,
        "a client outside network.exposeFrom reached the VM pod"
    );

    // A fork gets the same ports and its own Service, which reaches the fork; the
    // original's Service still reaches the original.
    let out = savestate(&["fork", "site", "-n", "1"]);
    let fork = out
        .lines()
        .find_map(|line| line.strip_prefix("sandbox ")?.split(' ').next())
        .expect("fork names its sandbox")
        .to_owned();
    let fork_url = format!("{}:8080", service_ip(&services, &fork).await);
    wait_for("the fork's Service to answer", 120, || async {
        fetch(&client, inside, &fork_url).await.as_deref() == Some(fork.as_str())
    })
    .await;
    assert_eq!(
        fetch(&client, inside, &site_url).await.as_deref(),
        Some("site")
    );
    assert_eq!(
        fetch(&client, outside, &fork_url).await,
        None,
        "a client outside network.exposeFrom reached the fork"
    );

    let checkpoint = fork.rsplit_once('-').unwrap().0.to_owned();
    for sandbox in ["site", fork.as_str()] {
        savestate(&["delete", "sandbox", sandbox]);
    }
    savestate(&["delete", "checkpoint", &checkpoint]);
    for (namespace, name) in [inside, outside] {
        Api::<Pod>::namespaced(client.clone(), namespace)
            .delete(name, &DeleteParams::default())
            .await
            .unwrap();
    }
}

/// Starts a pod running `image` (which has bash and coreutils) in `namespace`, and
/// waits until it runs.
async fn client_pod(client: &Client, namespace: &str, name: &str, image: &str) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": name},
        "spec": {
            "containers": [{"name": "client", "image": image, "command": ["sleep", "infinity"]}],
            "terminationGracePeriodSeconds": 0,
        },
    }))
    .unwrap();
    pods.create(&PostParams::default(), &pod).await.unwrap();
    wait_for(&format!("pod {namespace}/{name}"), 120, || async {
        pods.get(name)
            .await
            .unwrap()
            .status
            .and_then(|s| s.phase)
            .as_deref()
            == Some("Running")
    })
    .await;
}

/// One HTTP GET of `addr` (`IP:PORT`) from a client pod: the answer's last line,
/// or `None` if it could not connect or got no answer within 5 s.
async fn fetch(client: &Client, (namespace, pod): (&str, &str), addr: &str) -> Option<String> {
    let (ip, port) = addr.split_once(':').unwrap();
    let script = format!(
        "exec 3<>/dev/tcp/{ip}/{port} && printf 'GET / HTTP/1.0\\r\\n\\r\\n' >&3 && cat <&3"
    );
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params = AttachParams::default().container("client").stderr(false);
    let mut process = pods
        .exec(pod, ["timeout", "5", "bash", "-c", &script], &params)
        .await
        .unwrap();
    let status = process.take_status().unwrap();
    let mut out = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut process.stdout().unwrap(), &mut out)
        .await
        .unwrap();
    let ok = status.await.and_then(|s| s.status).as_deref() == Some("Success");
    ok.then(|| out.lines().last().unwrap_or_default().to_owned())
}

/// The same GET from this process.
fn fetch_local(addr: &str) -> Option<String> {
    use std::io::Read as _;
    let mut stream = std::net::TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    out.lines().last().map(str::to_owned)
}

async fn pod_ip(pods: &Api<Pod>, name: &str) -> String {
    pods.get(name)
        .await
        .unwrap()
        .status
        .and_then(|s| s.pod_ip)
        .unwrap()
}

/// The `ClusterIP` of a sandbox's Service, once the controller has made it.
async fn service_ip(services: &Api<Service>, name: &str) -> String {
    wait_for(&format!("service {name}"), 60, || async {
        services.get_opt(name).await.unwrap().is_some()
    })
    .await;
    services
        .get(name)
        .await
        .unwrap()
        .spec
        .and_then(|s| s.cluster_ip)
        .unwrap()
}

async fn scale_down(client: &Client, node: &str) -> Option<String> {
    Api::<Node>::all(client.clone())
        .get(node)
        .await
        .unwrap()
        .annotations()
        .get("cluster-autoscaler.kubernetes.io/scale-down-disabled")
        .cloned()
}

/// What the store holds for checkpoint `uid`: bytes per file, and the manifest.
async fn store_objects(uid: &str) -> String {
    use object_store::ObjectStore as _;
    let endpoint = env("SAVESTATE_E2E_S3_ENDPOINT");
    let store = object_store::aws::AmazonS3Builder::new()
        .with_endpoint(&endpoint)
        .with_bucket_name(env("SAVESTATE_E2E_S3_BUCKET"))
        .with_region("us-east-1")
        .with_access_key_id(env("SAVESTATE_E2E_S3_ACCESS_KEY_ID"))
        .with_secret_access_key(env("SAVESTATE_E2E_S3_SECRET_ACCESS_KEY"))
        .with_allow_http(endpoint.starts_with("http://"))
        .build()
        .unwrap();
    let prefix = env("SAVESTATE_E2E_S3_PREFIX");
    let objects: Vec<_> = futures::TryStreamExt::try_collect(
        store.list(Some(&object_store::path::Path::from(prefix.as_str()))),
    )
    .await
    .unwrap();
    let mut files = std::collections::BTreeMap::<String, (u64, usize)>::new();
    for object in objects {
        let key = object.location.to_string();
        let rest = key.strip_prefix(&format!("{prefix}/")).unwrap_or(&key);
        let group = match rest
            .strip_prefix("files/")
            .and_then(|path| path.rsplit_once('/'))
        {
            Some((path, _)) => path.replace(uid, "<UID>"),
            None => rest.replace(uid, "<UID>"),
        };
        let entry = files.entry(group).or_default();
        entry.0 += object.size;
        entry.1 += 1;
    }
    files
        .iter()
        .map(|(path, (bytes, objects))| {
            format!("{path}: {:.1} MiB in {objects} objects", f64_mib(*bytes))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[expect(clippy::cast_precision_loss, reason = "bytes as MiB for a report")]
fn f64_mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 20)
}

async fn wait_for<F: Future<Output = bool>>(what: &str, seconds: u64, done: impl Fn() -> F) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_savestate"))
}

/// Runs a verb; its stdout. Panics with its output if it fails.
fn savestate(args: &[&str]) -> String {
    let out = cli().args(args).output().unwrap();
    assert!(
        out.status.success(),
        "savestate {args:?}: {}",
        describe(&out)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    report(&format!(
        "$ savestate {}\n{}",
        args.join(" "),
        stdout.trim_end()
    ));
    stdout
}

/// Runs a verb with `-o json`; its output.
fn json(args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&savestate(args)).unwrap()
}

/// The names of a JSON list of objects, in order.
fn names(objects: &serde_json::Value) -> Vec<String> {
    objects
        .as_array()
        .unwrap()
        .iter()
        .map(|object| object["metadata"]["name"].as_str().unwrap().to_owned())
        .collect()
}

/// An object's labels, in key order.
fn labels(object: &serde_json::Value) -> Vec<(&str, &str)> {
    object["metadata"]["labels"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str().unwrap()))
        .collect()
}

/// `sh -c script` in a sandbox.
fn exec(sandbox: &str, script: &str) -> Output {
    cli()
        .args(["exec", sandbox, "--", "sh", "-c", script])
        .output()
        .unwrap()
}

/// `sh -c script` in a sandbox, which must succeed; its stdout.
fn sh(sandbox: &str, script: &str) -> String {
    let out = exec(sandbox, script);
    assert!(
        out.status.success(),
        "{sandbox}: {script}: {}",
        describe(&out)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Memory a sandbox's guest uses besides what it can reclaim, such as its page cache:
/// `MemTotal` minus `MemAvailable`.
fn guest_used_bytes(sandbox: &str) -> u64 {
    let meminfo = sh(sandbox, "cat /proc/meminfo");
    let kib = |field: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    (kib("MemTotal:") - kib("MemAvailable:")) << 10
}

/// The counting process's pid and count.
fn counter(sandbox: &str) -> (u32, u64) {
    let line = sh(sandbox, "cat /run/counter");
    let (pid, count) = line.trim().split_once(' ').unwrap();
    (pid.parse().unwrap(), count.parse().unwrap())
}

fn describe(out: &Output) -> String {
    format!(
        "{}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The node agent's `Saved` event on a checkpoint: how long the sandbox paused.
async fn saved_note(client: &Client, ns: &str, checkpoint: &str) -> String {
    note(client, ns, checkpoint, "Saved").await
}

/// The note of the latest event `reason` on the object named `name`.
async fn note(client: &Client, ns: &str, name: &str, reason: &str) -> String {
    let events: Api<Event> = Api::namespaced(client.clone(), ns);
    events
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| {
            event.reason.as_deref() == Some(reason)
                && event.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some(name)
        })
        .max_by_key(|event| event.metadata.creation_timestamp.clone().map(|time| time.0))
        .and_then(|event| event.note)
        .unwrap_or_else(|| format!("no {reason} event"))
}

/// The peak memory of a VM pod's launcher container (QEMU, the launcher and the
/// kernel memory charged to them, such as KVM's).
async fn peak_mib(client: &Client, ns: &str, pod: &str) -> u64 {
    let out = sh_pod(client, ns, pod, "cat /sys/fs/cgroup/memory.peak").await;
    out.trim().parse::<u64>().unwrap() >> 20
}

/// `sh -c script` in a VM pod's launcher container (not the guest); its stdout.
async fn sh_pod(client: &Client, ns: &str, pod: &str, script: &str) -> String {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let params = AttachParams::default().container("launcher").stderr(false);
    let mut process = pods.exec(pod, ["sh", "-c", script], &params).await.unwrap();
    let mut out = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut process.stdout().unwrap(), &mut out)
        .await
        .unwrap();
    process.join().await.unwrap();
    out
}

fn secs(since: Instant) -> f64 {
    since.elapsed().as_secs_f64()
}

fn unix_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// A result line, also appended to `SAVESTATE_E2E_REPORT` when set.
fn report(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "E2E: {line}");
    if let Ok(path) = std::env::var("SAVESTATE_E2E_REPORT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

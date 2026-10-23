//! savestate end to end on a real cluster with KVM: the CLI creates a sandbox from a
//! base image, state is written into it, it is checkpointed while it keeps running,
//! two sandboxes start from the checkpoint (one with a frozen clock, one stepped),
//! and each is shown to have resumed exactly the checkpoint's state, independently of
//! the other and of the original, which carries on unaffected. A sandbox forked from
//! a restored one carries both histories.
//!
//! Needs a disposable cluster in `KUBECONFIG` with savestate installed and at least
//! three 1-vCPU VMs' room on one node (CI: kind on a KVM runner), and:
//!
//! - `SAVESTATE_E2E_IMAGE`: the base image (`…@sha256:…`) holding `/rootfs.qcow2`
//! - `SAVESTATE_NAMESPACE`: a sandbox namespace from the installation config
//! - `SAVESTATE_E2E_ALLOWED`: `IP:PORT` the egress allowlist lets guests reach
//! - `SAVESTATE_E2E_BLOCKED`: `IP:PORT` in a private range guests must not reach

// clippy.toml allows these in `#[test]` functions only, not in a test file's helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::Write as _,
    process::{Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use k8s_openapi::api::{
    core::v1::{Node, Pod},
    events::v1::Event,
};
use kube::{
    Api, Client,
    api::{AttachParams, ListParams},
};
use savestate_api::Checkpoint;

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
        "create", "web", "--image", &image, "--vcpus", "1", "--memory", "1Gi", "--disk", "4Gi",
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
    let allocatable = &node.status.unwrap().allocatable.unwrap()["cpu"].0;
    report(&format!("node allocatable CPU: {allocatable}"));
    savestate(&["delete", "sandbox", "burst"]);

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

    // State: a process counting in its memory, and a file on disk.
    sh(
        "web",
        "setsid sh -c 'i=0; while :; do i=$((i+1)); echo \"$$ $i\" >/run/counter.new && mv /run/counter.new /run/counter; sleep 0.1; done' </dev/null >/dev/null 2>&1 &",
    );
    sh("web", "echo before >/root/state && sync");
    std::thread::sleep(Duration::from_secs(2));
    let (pid, at_checkpoint) = counter("web");
    let machine_id = sh("web", "cat /etc/machine-id");

    // Checkpoint while it runs, then change the original.
    let started = Instant::now();
    savestate(&["checkpoint", "web", "--name", "web-1"]);
    report(&format!(
        "checkpoint: create to Ready in {:.1} s; {}",
        secs(started),
        saved_note(&client, &ns, "web-1").await
    ));
    let ready = checkpoints.get("web-1").await.unwrap().status.unwrap();
    let guest_time = ready.guest_time.unwrap().0.as_second();
    assert_eq!(ready.vcpus, Some(1));
    assert_eq!(ready.memory.unwrap().0, "1Gi");
    assert_eq!(ready.parent, None);
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
    let out = savestate(&["fork", "web-frozen", "-n", "1"]);
    let grandchild = out
        .lines()
        .find_map(|line| line.strip_prefix("sandbox ")?.split(' ').next())
        .expect("fork names its sandbox")
        .to_owned();
    let checkpoint = grandchild.rsplit_once('-').unwrap().0;
    let status = checkpoints.get(checkpoint).await.unwrap().status.unwrap();
    assert_eq!(status.parent.as_deref(), Some("web-1"));
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
    let events: Api<Event> = Api::namespaced(client.clone(), ns);
    events
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|event| {
            event.reason.as_deref() == Some("Saved")
                && event.regarding.as_ref().and_then(|r| r.name.as_deref()) == Some(checkpoint)
        })
        .and_then(|event| event.note)
        .unwrap_or_else(|| "no Saved event".into())
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

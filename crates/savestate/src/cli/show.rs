//! Text output of `list` and `get`: tables and fields for people. Scripts use
//! `-o json`, which prints the objects themselves.

use std::{collections::BTreeMap, io::Write as _};

use k8s_openapi::apimachinery::pkg::{api::resource::Quantity, apis::meta::v1::Time};
use kube::ResourceExt as _;
use savestate_api::{Checkpoint, Sandbox};

const NONE: &str = "<none>";

pub fn sandboxes(items: &[Sandbox]) -> anyhow::Result<()> {
    let rows = items.iter().map(|sandbox| {
        let status = sandbox.status.clone().unwrap_or_default();
        vec![
            sandbox.name_any(),
            format!("{:?}", status.phase),
            status.node.unwrap_or_default(),
            sandbox
                .spec
                .from
                .as_ref()
                .map(|from| from.checkpoint.clone())
                .unwrap_or_default(),
            age(sandbox.creation_timestamp().as_ref()),
        ]
    });
    table(&["NAME", "PHASE", "NODE", "FROM", "AGE"], rows)
}

pub fn checkpoints(items: &[Checkpoint]) -> anyhow::Result<()> {
    let rows = items.iter().map(|checkpoint| {
        let status = checkpoint.status.clone().unwrap_or_default();
        vec![
            checkpoint.name_any(),
            format!("{:?}", status.phase),
            checkpoint.spec.sandbox.clone(),
            status.parent.unwrap_or_default(),
            status.nodes.join(","),
            status.stores.join(","),
            status.memory_bytes.map(bytes).unwrap_or_default(),
            status.disk_bytes.map(bytes).unwrap_or_default(),
            age(checkpoint.creation_timestamp().as_ref()),
        ]
    });
    table(
        &[
            "NAME", "PHASE", "SANDBOX", "PARENT", "NODES", "STORES", "MEMORY", "DISK", "AGE",
        ],
        rows,
    )
}

/// One sandbox; `from` is the checkpoint it started from, for its shape.
pub fn sandbox(sandbox: &Sandbox, from: Option<&Checkpoint>) -> anyhow::Result<()> {
    let status = sandbox.status.clone().unwrap_or_default();
    let spec = &sandbox.spec;
    let source = match (&spec.image, &spec.from) {
        (_, Some(from)) => format!(
            "checkpoint {} (clock {})",
            from.checkpoint,
            format!("{:?}", spec.clock.unwrap_or_default()).to_lowercase()
        ),
        (Some(image), None) => format!("image {image}"),
        (None, None) => String::new(),
    };
    let shape = match from.and_then(|checkpoint| checkpoint.status.as_ref()) {
        Some(from) if spec.from.is_some() => {
            shape(from.vcpus, from.memory.as_ref(), from.disk.as_ref())
        }
        _ => shape(spec.vcpus, spec.memory.as_ref(), spec.disk.as_ref()),
    };
    fields(&[
        ("Sandbox", sandbox.name_any()),
        ("Phase", format!("{:?}", status.phase)),
        ("Node", status.node.unwrap_or_default()),
        ("Source", source),
        ("Shape", shape),
        (
            "TTL",
            spec.ttl
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        ),
        ("Age", age(sandbox.creation_timestamp().as_ref())),
        ("Labels", labels(sandbox.labels())),
        ("Message", status.message.unwrap_or_default()),
    ])
}

pub fn checkpoint(checkpoint: &Checkpoint) -> anyhow::Result<()> {
    let status = checkpoint.status.clone().unwrap_or_default();
    let spec = &checkpoint.spec;
    let list = |items: &[String]| {
        if items.is_empty() {
            NONE.to_owned()
        } else {
            items.join(", ")
        }
    };
    fields(&[
        ("Checkpoint", checkpoint.name_any()),
        ("Phase", format!("{:?}", status.phase)),
        ("Sandbox", spec.sandbox.clone()),
        ("Parent", status.parent.unwrap_or_else(|| NONE.into())),
        ("Nodes", list(&status.nodes)),
        ("Stores", list(&status.stores)),
        ("Export", spec.export.clone().unwrap_or_default()),
        (
            "Shape",
            shape(status.vcpus, status.memory.as_ref(), status.disk.as_ref()),
        ),
        (
            "Memory file",
            status.memory_bytes.map(bytes).unwrap_or_default(),
        ),
        (
            "Disk layer",
            status.disk_bytes.map(bytes).unwrap_or_default(),
        ),
        (
            "Guest time",
            status
                .guest_time
                .map(|time| time.0.to_string())
                .unwrap_or_default(),
        ),
        (
            "TTL",
            spec.ttl
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        ),
        ("Age", age(checkpoint.creation_timestamp().as_ref())),
        ("Labels", labels(checkpoint.labels())),
        ("Message", status.message.unwrap_or_default()),
    ])
}

fn shape(vcpus: Option<u32>, memory: Option<&Quantity>, disk: Option<&Quantity>) -> String {
    match (vcpus, memory, disk) {
        (Some(vcpus), Some(memory), Some(disk)) => format!(
            "{vcpus} vCPU{}, {} memory, {} disk",
            if vcpus == 1 { "" } else { "s" },
            memory.0,
            disk.0
        ),
        _ => String::new(),
    }
}

fn labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return NONE.into();
    }
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bytes in binary units, e.g. `170.2Mi`.
#[expect(clippy::cast_precision_loss, reason = "a size for people")]
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["", "Ki", "Mi", "Gi", "Ti"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n}")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// Time since `created` in its largest whole unit, e.g. `5m` or `3d`.
fn age(created: Option<&Time>) -> String {
    let Some(created) = created else {
        return String::new();
    };
    let seconds = jiff::Timestamp::now()
        .duration_since(created.0)
        .as_secs()
        .max(0);
    if seconds < 120 {
        format!("{seconds}s")
    } else if seconds < 7_200 {
        format!("{}m", seconds / 60)
    } else if seconds < 172_800 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Columns padded to their widest cell.
fn table(header: &[&str], rows: impl Iterator<Item = Vec<String>>) -> anyhow::Result<()> {
    let mut rows: Vec<Vec<String>> = rows.collect();
    rows.insert(0, header.iter().map(ToString::to_string).collect());
    let mut widths = vec![0; header.len()];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = std::io::stdout().lock();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        writeln!(out, "{}", line.join("   ").trim_end())?;
    }
    Ok(())
}

/// `Name: value` lines, values aligned; empty values are left out.
fn fields(fields: &[(&str, String)]) -> anyhow::Result<()> {
    let width = fields.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 1;
    let mut out = std::io::stdout().lock();
    for (name, value) in fields {
        if !value.is_empty() {
            writeln!(out, "{:<width$} {value}", format!("{name}:"))?;
        }
    }
    Ok(())
}

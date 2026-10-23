# savestate

Read `docs/design.md` first. It is the contract: the user-facing behaviour, the CRDs,
the architecture, the QEMU mechanics and the phases. Change it in the same pull
request as any change to the behaviour it describes.

## Rules

- This repository is public and describes only itself. Do not name, compare with or
  reference any other sandbox, VM or checkpoint platform, product or internal system
  in code, comments, docs, commit messages, branch names or pull requests. QEMU, KVM,
  Linux, Kubernetes and orbd are fine.
- Do not reference private infrastructure: no internal hostnames, node names, IP
  addresses, cluster names or organisation-internal URLs.
- Keep the configuration boundary in `docs/design.md`: the repository holds general
  behaviour, schemas, generic defaults and placeholder examples. Anything specific to
  one installation (node names, CPU model choice, VM shapes, images and what they
  contain, registries, egress rules, stores, credentials) is installation config,
  never a hard-coded value or a committed default. Do not describe what any
  particular user runs inside their VMs; examples use neutral names (`web-1`,
  `node-a`, `ghcr.io/example/...`).
- Follow the structure and conventions of [samcm/orbd](https://github.com/samcm/orbd):
  one Cargo workspace under `crates/`, edition 2024, `publish = false`, one static
  binary, a guest directory for the guest kernel and rootfs, GitHub Actions for CI,
  images on ghcr.io and release binaries, MIT licence, and a README in orbd's style.
- QEMU, QMP and guest-agent code is shared with orbd through crates in orbd's
  workspace, consumed as a git dependency pinned to a revision. Improve the shared
  code there (by pull request to orbd) rather than copying it here.
- Drive stock QEMU over QMP. Do not patch QEMU or the host kernel.
- Work through pull requests; keep each one to one coherent change.

## Tests

- Add a test only when you can name the realistic regression it catches, and that
  regression would hurt a user: a restore that resumes the wrong state, copies that
  are not independent, a crash that leaves a half-published checkpoint, a controller
  that deletes data still in use, a CRD that accepts a spec that cannot work. If you
  cannot name it, do not write the test.
- Do not test trivia or the improbable: serde round-trips, argument formatting,
  constants, getters, `Display` text, log lines, mock call order, or states the code
  cannot reach.
- Prefer one end-to-end test against real QEMU/KVM over many unit tests of glue code.
  Unit tests are for logic with real edge cases (layer chains, GC reachability,
  placement decisions).
- Test through public interfaces so tests survive refactors.

## No compatibility, no migrations

Nothing is deployed. Make hard, clean breaking changes:

- Change CRD fields, config, on-disk layout, checkpoint formats and wire protocols in
  place. No version shims, fallback readers for old formats, deprecated aliases,
  conversion webhooks or migration code, and no tests of upgrades from older versions.
  A checkpoint from an older build may simply be refused; its fingerprint already
  says so.
- Delete the code and docs a change replaces in the same pull request.
- This holds until `docs/design.md` says savestate has users to protect.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Tests that need `/dev/kvm` are ignored by default and run in CI on GitHub-hosted
runners, as in orbd.

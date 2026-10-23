#!/usr/bin/env bash
# Builds savestate's guest kernel: the latest stable Linux series, configured from
# x86_64_defconfig and kvm_guest.config, then orbd's guest config fragment (at the orbd
# revision Cargo.lock pins), then savestate.config, which wins where both set a symbol.
# Everything is built in: no modules, no initrd.
#
#   guest/kernel/build.sh [OUT_DIR]        (default: target/guest-kernel)
#
# Outputs in OUT_DIR:
#   bzImage          the kernel, booted directly by QEMU (`-kernel`)
#   config           the final .config
#   bzImage.sha256   sha256 of bzImage
#   manifest.txt     kernel version, source and fragment digests, final config digest
#
# Needs: curl, sha256sum, tar, xz, gcc, make, flex, bison, bc, libelf-dev, libssl-dev.
# SAVESTATE_KERNEL_CACHE caches the kernel tarball (default: ~/.cache/savestate-kernel).
set -euo pipefail

KERNEL_VERSION=7.2.12
KERNEL_SHA256=bb18468a8dc8978aa00caa6446e753f7b6da3fee56b6b7f38b16f9430ba02d4f
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_VERSION%%.*}.x/linux-${KERNEL_VERSION}.tar.xz"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$(realpath -m "${1:-$repo/target/guest-kernel}")"
cache="${SAVESTATE_KERNEL_CACHE:-$HOME/.cache/savestate-kernel}"
tarball="$cache/linux-${KERNEL_VERSION}.tar.xz"
src="$out/build/linux-${KERNEL_VERSION}"
fragment="$out/build/fragment.config"

mkdir -p "$cache" "$out/build"

if ! echo "$KERNEL_SHA256  $tarball" | sha256sum -c --status 2>/dev/null; then
    echo "downloading $KERNEL_URL"
    curl -fL --retry 3 -o "$tarball.part" "$KERNEL_URL"
    echo "$KERNEL_SHA256  $tarball.part" | sha256sum -c --quiet
    mv "$tarball.part" "$tarball"
fi

# orbd's fragment without the symbols savestate.config sets, then savestate.config.
orbd_rev="$("$here/orbd-rev.sh")"
curl -fsSL --retry 3 -o "$out/build/orbd.config" \
    "https://raw.githubusercontent.com/samcm/orbd/$orbd_rev/guest/kernel/orbd.config"
symbol() { sed -n 's/^\(CONFIG_[A-Za-z0-9_]*\)=.*/\1/p; s/^# \(CONFIG_[A-Za-z0-9_]*\) is not set$/\1/p'; }
ours="$(symbol <"$here/savestate.config")"
while IFS= read -r line; do
    sym="$(printf '%s\n' "$line" | symbol)"
    if [ -z "$sym" ] || ! grep -qxF "$sym" <<<"$ours"; then
        printf '%s\n' "$line"
    fi
done <"$out/build/orbd.config" >"$fragment"
cat "$here/savestate.config" >>"$fragment"

rm -rf "$src"
tar -C "$out/build" -xf "$tarball"

# Reproducible build metadata (no host name, user or wall-clock time in the image).
export KBUILD_BUILD_USER=savestate KBUILD_BUILD_HOST=savestate
export KBUILD_BUILD_TIMESTAMP="1970-01-01 00:00:00 UTC" KBUILD_BUILD_VERSION=1

make -C "$src" -s x86_64_defconfig
make -C "$src" -s kvm_guest.config
"$src/scripts/kconfig/merge_config.sh" -m -O "$src" "$src/.config" "$fragment" >/dev/null
make -C "$src" -s olddefconfig

# Every option in the fragment must have survived dependency resolution.
missing=0
while IFS= read -r line; do
    case "$line" in
        CONFIG_*=*)
            if ! grep -qxF "$line" "$src/.config"; then
                echo "config: wanted $line, got: $(grep -E "^(# )?${line%%=*}[= ]" "$src/.config" || echo unset)" >&2
                missing=1
            fi
            ;;
        "# CONFIG_"*" is not set")
            sym="${line#\# }"
            sym="${sym%% *}"
            if grep -qE "^${sym}=" "$src/.config"; then
                echo "config: wanted $sym unset, got: $(grep -E "^${sym}=" "$src/.config")" >&2
                missing=1
            fi
            ;;
    esac
done <"$fragment"
[ "$missing" = 0 ] || { echo "config fragment not satisfied" >&2; exit 1; }

make -C "$src" -s -j"$(nproc)" bzImage

cp "$src/arch/x86/boot/bzImage" "$out/bzImage.tmp"
cp "$src/.config" "$out/config"
mv -f "$out/bzImage.tmp" "$out/bzImage"
kernel_sha="$(sha256sum "$out/bzImage" | cut -d' ' -f1)"
echo "$kernel_sha  bzImage" >"$out/bzImage.sha256"
cat >"$out/manifest.txt" <<EOF
kernel_version=${KERNEL_VERSION}
kernel_release=$(make -C "$src" -s kernelrelease)
source_url=${KERNEL_URL}
source_sha256=${KERNEL_SHA256}
orbd_rev=${orbd_rev}
fragment_sha256=$(sha256sum "$fragment" | cut -d' ' -f1)
config_sha256=$(sha256sum "$out/config" | cut -d' ' -f1)
bzImage_sha256=${kernel_sha}
EOF
cat "$out/manifest.txt"

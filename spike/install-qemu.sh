#!/usr/bin/env bash
# Install QEMU 11.1 and the spike's tools in a Debian 13 (trixie) container.
#
# The design pins QEMU 11.1 from trixie-backports. If the backports suite does not
# (yet) carry an 11.1 binary for this architecture, the same Debian source package is
# taken from forky (testing) instead, and the script says so; RESULTS.md records which.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
want=${QEMU_WANT:-11.1}

cat >/etc/apt/sources.list.d/extra.sources <<'EOF'
Types: deb
URIs: http://deb.debian.org/debian
Suites: trixie-backports
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb
URIs: http://deb.debian.org/debian
Suites: forky
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
EOF
# Only pull from forky what is asked for explicitly.
printf 'Package: *\nPin: release n=forky\nPin-Priority: 100\n' >/etc/apt/preferences.d/forky

apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates curl python3 jq genisoimage procps util-linux coreutils kmod >/dev/null

suite=trixie-backports
bpo=$(apt-cache madison qemu-system-x86 | awk -v s=trixie-backports '$0 ~ s {print $3; exit}')
case "$bpo" in
  *:"$want".*) ;;
  *)
    echo "::warning title=QEMU source::trixie-backports has qemu-system-x86 '${bpo:-none}', not $want; using forky" >&2
    suite=forky
    ;;
esac
apt-get install -y -qq --no-install-recommends -t "$suite" qemu-system-x86 qemu-utils >/dev/null

qemu-system-x86_64 --version | head -1
dpkg-query -W -f='qemu-system-x86 ${Version} (from '"$suite"')\n' qemu-system-x86 | tee /etc/qemu-spike-source
qemu-system-x86_64 -machine help | grep -q '^pc-q35-11.1 ' || {
  echo "machine type pc-q35-11.1 missing" >&2
  exit 1
}

# Guest networking for the spike, sourced by lib.sh and prepare-guest.sh. Needs root.
#
# The launcher's QEMU has no user-mode (slirp) backend; savestate gives each VM a tap
# device with nftables NAT (docs/design.md, "Network and security"). The spike does
# the same in a simplified form: one bridge in the current network namespace,
# 10.0.2.2/24, masqueraded out. Guests use the static address 10.0.2.15/24, written
# into the image by prepare-guest.sh. Only one VM at a time uses it for traffic; VMs
# that do not need the network get a NIC on an empty hub instead (NET=none).

SPIKE_BRIDGE=br-spike

net_up() {
  ip link show "$SPIKE_BRIDGE" >/dev/null 2>&1 && return 0
  ip link add "$SPIKE_BRIDGE" type bridge
  ip addr add 10.0.2.2/24 dev "$SPIKE_BRIDGE"
  ip link set "$SPIKE_BRIDGE" up
  sysctl -qw net.ipv4.ip_forward=1
  nft -f - <<'EOF'
table ip spike {
  chain postrouting {
    type nat hook postrouting priority srcnat;
    ip saddr 10.0.2.0/24 oifname != "br-spike" masquerade
  }
}
EOF
}

# The first non-loopback IPv4 resolver this namespace uses (else a public one), for the guest.
net_dns() {
  awk '$1 == "nameserver" && $2 !~ /^127\./ && $2 !~ /:/ { print $2; found = 1; exit } END { if (!found) print "9.9.9.9" }' /etc/resolv.conf
}

# net_args NAME: the -netdev argument for VM NAME; creates its tap when NET=tap.
net_args() {
  if [ "${NET:-none}" = tap ]; then
    local tap
    tap=t-$(echo "$1" | tr -cd 'a-z0-9-' | tail -c 13)
    net_up
    ip link del "$tap" 2>/dev/null || true
    ip tuntap add dev "$tap" mode tap
    ip link set "$tap" master "$SPIKE_BRIDGE" up
    echo "tap,id=net0,ifname=$tap,script=no,downscript=no"
  else
    echo "hubport,id=net0,hubid=0"
  fi
}

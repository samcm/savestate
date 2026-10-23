//! The guest's network inside the VM pod's own network namespace (see "Network and
//! security" in `docs/design.md`): a tap device QEMU attaches to, IPv4 forwarding,
//! and nftables that masquerade guest traffic and drop what it must not reach.
//!
//! Every guest has the same address, [`GUEST_ADDR`] behind [`GATEWAY`], and the same
//! MAC; each VM pod is its own namespace, so they never meet. The launcher builds this
//! once, as root, before it drops its privileges and starts QEMU.

use std::{
    borrow::Cow,
    fmt::Write as _,
    net::{IpAddr, Ipv4Addr},
    process::Command,
};

use anyhow::{Context as _, bail};
use futures::TryStreamExt as _;
use ipnet::{IpNet, Ipv4Net};
use nftables::{
    batch::Batch,
    expr::{
        CT, Expression, Meta, MetaKey, NamedExpression, Payload, PayloadField, Prefix, SetItem,
    },
    schema::{Chain, NfListObject, Rule, Table},
    stmt::{Match, Operator, Statement},
    types::{NfChainPolicy, NfChainType, NfFamily, NfHook},
};
use rtnetlink::{
    LinkUnspec,
    packet_route::link::{AfSpecInet, AfSpecUnspec, InetDevConf, LinkAttribute},
};

use crate::store::{QEMU_GID, QEMU_UID};

/// The tap device QEMU attaches the guest's NIC to.
pub const TAP: &str = "tap0";
/// The guest's NIC.
pub const MAC: &str = "52:54:00:12:34:56";
pub const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const PREFIX_LEN: u8 = 24;
const TABLE: &str = "savestate";

/// Destinations guests never reach unless allowed: private, shared (CGNAT),
/// loopback, link-local (cloud metadata), multicast and reserved ranges, where
/// cluster, node and pod addresses live.
const BLOCKED: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// What the guest may reach besides the public internet.
pub struct Egress<'a> {
    /// Allowed in blocked ranges.
    pub allow: &'a [IpNet],
    /// DNS servers: port 53 only.
    pub dns: &'a [Ipv4Addr],
    /// Also blocked: the node's own addresses (which may be public).
    pub host: &'a [IpAddr],
}

/// Builds the tap device, forwarding and the nftables table. Needs `NET_ADMIN`.
pub async fn setup(egress: &Egress<'_>) -> anyhow::Result<()> {
    // Creating a tap needs the TUNSETIFF ioctl, which netlink does not offer.
    let status = Command::new("ip")
        .args(["tuntap", "add", "dev", TAP, "mode", "tap"])
        .args([
            "user",
            &QEMU_UID.to_string(),
            "group",
            &QEMU_GID.to_string(),
        ])
        .status()
        .context("ip tuntap add")?;
    if !status.success() {
        bail!("ip tuntap add dev {TAP}: {status}");
    }

    let (connection, handle, _) = rtnetlink::new_connection().context("netlink")?;
    tokio::spawn(connection);
    let links: Vec<_> = handle.link().get().execute().try_collect().await?;
    let mut tap = None;
    for link in links {
        let name = link.attributes.iter().find_map(|a| match a {
            LinkAttribute::IfName(name) => Some(name.clone()),
            _ => None,
        });
        if name.as_deref() == Some("lo") {
            continue;
        }
        if name.as_deref() == Some(TAP) {
            tap = Some(link.header.index);
        }
        // Per-interface IPv4 forwarding (what `net.ipv4.ip_forward` sets on all of
        // them), through netlink: /proc/sys is read-only in containers.
        let mut conf = InetDevConf::default();
        conf.forwarding = 1;
        let message = LinkUnspec::new_with_index(link.header.index)
            .append_extra_attribute(LinkAttribute::AfSpecUnspec(vec![AfSpecUnspec::Inet(vec![
                AfSpecInet::DevConfRequest(conf),
            ])]))
            .build();
        handle
            .link()
            .set(message)
            .execute()
            .await
            .with_context(|| format!("enabling forwarding on {name:?}"))?;
    }
    let tap = tap.with_context(|| format!("{TAP} did not appear"))?;
    handle
        .address()
        .add(tap, IpAddr::V4(GATEWAY), PREFIX_LEN)
        .execute()
        .await
        .context("addressing the tap device")?;
    handle
        .link()
        .set(LinkUnspec::new_with_index(tap).up().build())
        .execute()
        .await
        .context("bringing the tap device up")?;

    nftables::helper::apply_ruleset(&ruleset(egress)).context("nft")?;
    Ok(())
}

fn ruleset(egress: &Egress<'_>) -> nftables::schema::Nftables<'static> {
    let mut batch = Batch::new();
    batch.add(NfListObject::Table(Table {
        family: NfFamily::INet,
        name: TABLE.into(),
        handle: None,
    }));
    for (name, type_, hook, prio) in [
        ("input", NfChainType::Filter, NfHook::Input, 0),
        ("forward", NfChainType::Filter, NfHook::Forward, 0),
        ("postrouting", NfChainType::NAT, NfHook::Postrouting, 100),
    ] {
        batch.add(NfListObject::Chain(Chain {
            family: NfFamily::INet,
            table: TABLE.into(),
            name: name.into(),
            _type: Some(type_),
            hook: Some(hook),
            prio: Some(prio),
            policy: Some(NfChainPolicy::Accept),
            ..Chain::default()
        }));
    }
    let v4 = |nets: &mut dyn Iterator<Item = IpNet>| -> Vec<Ipv4Net> {
        nets.filter_map(|net| match net {
            IpNet::V4(net) => Some(net),
            IpNet::V6(_) => None,
        })
        .collect()
    };
    let allow = v4(&mut egress.allow.iter().copied());
    let dns: Vec<Ipv4Net> = egress.dns.iter().map(|ip| Ipv4Net::from(*ip)).collect();
    let mut blocked = v4(&mut BLOCKED.iter().filter_map(|net| net.parse().ok()));
    blocked.extend(v4(&mut egress.host.iter().map(|ip| IpNet::from(*ip))));

    let mut rules = vec![
        // The guest reaches nothing in the pod itself (the launcher's socket is a
        // unix socket), except replies.
        ("input", vec![from_guest(), established(), accept()]),
        ("input", vec![from_guest(), drop()]),
        // No IPv6 at all.
        (
            "forward",
            vec![
                from_guest(),
                is(meta(MetaKey::Nfproto), Expression::String("ipv6".into())),
                drop(),
            ],
        ),
    ];
    if !allow.is_empty() {
        rules.push(("forward", vec![from_guest(), daddr(allow), accept()]));
    }
    if !dns.is_empty() {
        let l4 = ["tcp", "udp"]
            .map(|proto| SetItem::Element(Expression::String(proto.into())))
            .into();
        rules.push((
            "forward",
            vec![
                from_guest(),
                daddr(dns),
                is(
                    meta(MetaKey::L4proto),
                    Expression::Named(NamedExpression::Set(l4)),
                ),
                is(payload("th", "dport"), Expression::Number(53)),
                accept(),
            ],
        ));
    }
    rules.extend([
        ("forward", vec![from_guest(), daddr(blocked), drop()]),
        // Nothing reaches the guest unsolicited.
        ("forward", vec![to_guest(), established(), accept()]),
        ("forward", vec![to_guest(), drop()]),
        (
            "postrouting",
            vec![
                is(payload("ip", "saddr"), prefix(guest_net())),
                Statement::Match(Match {
                    left: meta(MetaKey::Oifname),
                    right: Expression::String(TAP.into()),
                    op: Operator::NEQ,
                }),
                Statement::Masquerade(None),
            ],
        ),
    ]);
    for (chain, expr) in rules {
        batch.add(NfListObject::Rule(Rule {
            family: NfFamily::INet,
            table: TABLE.into(),
            chain: chain.into(),
            expr: expr.into(),
            ..Rule::default()
        }));
    }
    batch.to_nftables()
}

fn from_guest() -> Statement<'static> {
    is(meta(MetaKey::Iifname), Expression::String(TAP.into()))
}

fn to_guest() -> Statement<'static> {
    is(meta(MetaKey::Oifname), Expression::String(TAP.into()))
}

/// `ct state { established, related }`
fn established() -> Statement<'static> {
    Statement::Match(Match {
        left: Expression::Named(NamedExpression::CT(CT {
            key: "state".into(),
            family: None,
            dir: None,
        })),
        right: Expression::List(vec![
            Expression::String("established".into()),
            Expression::String("related".into()),
        ]),
        op: Operator::IN,
    })
}

/// `ip daddr { nets }`
fn daddr(nets: Vec<Ipv4Net>) -> Statement<'static> {
    let set = nets
        .into_iter()
        .map(|net| SetItem::Element(prefix(net)))
        .collect();
    is(
        payload("ip", "daddr"),
        Expression::Named(NamedExpression::Set(set)),
    )
}

fn prefix(net: Ipv4Net) -> Expression<'static> {
    Expression::Named(NamedExpression::Prefix(Prefix {
        addr: Box::new(Expression::String(Cow::Owned(net.addr().to_string()))),
        len: u32::from(net.prefix_len()),
    }))
}

fn payload(protocol: &'static str, field: &'static str) -> Expression<'static> {
    Expression::Named(NamedExpression::Payload(Payload::PayloadField(
        PayloadField {
            protocol: protocol.into(),
            field: field.into(),
        },
    )))
}

fn accept() -> Statement<'static> {
    Statement::Accept(None)
}

fn drop() -> Statement<'static> {
    Statement::Drop(None)
}

/// 10.0.2.0/24
fn guest_net() -> Ipv4Net {
    Ipv4Net::new(GATEWAY, PREFIX_LEN).map_or(Ipv4Net::default(), |net| net.trunc())
}

fn meta(key: MetaKey) -> Expression<'static> {
    Expression::Named(NamedExpression::Meta(Meta { key }))
}

fn is(left: Expression<'static>, right: Expression<'static>) -> Statement<'static> {
    Statement::Match(Match {
        left,
        right,
        op: Operator::EQ,
    })
}

/// The IPv4 name servers in a `resolv.conf`.
pub fn nameservers(resolv_conf: &str) -> Vec<Ipv4Addr> {
    resolv_conf
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("nameserver")).then(|| words.next()?.parse().ok())?
        })
        .collect()
}

/// The script the launcher runs in a freshly booted guest: address, route, DNS and
/// hostname. A restored guest keeps what its checkpoint had.
pub fn guest_script(dns: &[Ipv4Addr], hostname: &str) -> String {
    let nameservers = dns.iter().fold(String::new(), |mut out, ip| {
        let _ = writeln!(out, "nameserver {ip}");
        out
    });
    format!(
        r#"set -eu
dev=$(ip -o link | awk -v mac={MAC} 'index($0, mac) {{ sub(/:$/, "", $2); print $2; exit }}')
[ -n "$dev" ] || {{ echo "no network interface with MAC {MAC}" >&2; exit 1; }}
ip link set "$dev" up
ip addr replace {GUEST_ADDR}/{PREFIX_LEN} dev "$dev"
ip route replace default via {GATEWAY}
printf '%s' '{nameservers}' >/etc/resolv.conf
echo {hostname} >/proc/sys/kernel/hostname
echo {hostname} >/etc/hostname
sed -i '/^127\.0\.1\.1[[:space:]]/d' /etc/hosts
echo '127.0.1.1 {hostname}' >>/etc/hosts
"#
    )
}

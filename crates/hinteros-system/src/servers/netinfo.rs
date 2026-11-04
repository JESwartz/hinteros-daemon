//! The box's address on its local network (the `url` of Jellyfin and Plex, Cloudflare's
//! `local_ip`), from `/proc/net/route` and `/proc/net/fib_trie`: no socket, no netlink (the
//! service may open neither).
//!
//! The address is the box's own IPv4 address in a subnet of the interface that carries the
//! default route (the lowest metric wins); without a default route, its first private address, else its first address that is not loopback or
//! link-local.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::Path;

/// One line of `/proc/net/route`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub iface: String,
    pub dest: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub metric: u32,
    /// `RTF_*` of `<linux/route.h>`.
    pub flags: u32,
    pub up: bool,
}

/// The kernel writes addresses as host-order hex numbers of the network-order bytes.
fn hex_addr(s: &str) -> Option<Ipv4Addr> {
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Ipv4Addr::from(n.to_ne_bytes()))
}

/// The lines of `/proc/net/route` (the one parser of it: `lanfilter` uses it too).
pub(crate) fn routes(text: &str) -> Vec<Route> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 8 {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            Some(Route {
                iface: f[0].to_owned(),
                dest: hex_addr(f[1])?,
                mask: hex_addr(f[7])?,
                metric: f[6].parse().ok()?,
                flags,
                up: flags & 1 == 1,
            })
        })
        .collect()
}

/// The box's own addresses (`/32 host LOCAL` leaves of `fib_trie`).
fn local_addrs(text: &str) -> BTreeSet<Ipv4Addr> {
    let mut out = BTreeSet::new();
    let mut leaf: Option<Ipv4Addr> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(addr) = t.strip_prefix("|-- ") {
            leaf = addr.trim().parse().ok();
        } else if t.starts_with('/') {
            if t.contains("host LOCAL")
                && let Some(a) = leaf
            {
                out.insert(a);
            }
        } else {
            leaf = None;
        }
    }
    out
}

fn usable(a: &Ipv4Addr) -> bool {
    !a.is_loopback() && !a.is_link_local() && !a.is_unspecified()
}

/// Pick the address from the two files' text.
pub fn pick(route: &str, fib_trie: &str) -> Option<Ipv4Addr> {
    let routes = routes(route);
    let locals: Vec<Ipv4Addr> = local_addrs(fib_trie).into_iter().filter(usable).collect();
    let default = routes
        .iter()
        .filter(|r| r.up && r.dest.is_unspecified() && r.mask.is_unspecified())
        .min_by_key(|r| r.metric);
    if let Some(d) = default {
        let in_subnet = |a: &Ipv4Addr| {
            routes.iter().any(|r| {
                r.up && r.iface == d.iface
                    && !r.mask.is_unspecified()
                    && u32::from(*a) & u32::from(r.mask) == u32::from(r.dest)
            })
        };
        if let Some(a) = locals.iter().find(|a| in_subnet(a)) {
            return Some(*a);
        }
    }
    locals
        .iter()
        .find(|a| a.is_private())
        .or(locals.first())
        .copied()
}

/// The box's local network address now (`proc_dir`: `/proc`).
pub fn local_ipv4(proc_dir: &Path) -> Option<Ipv4Addr> {
    let route = std::fs::read_to_string(proc_dir.join("net/route")).ok()?;
    let trie = std::fs::read_to_string(proc_dir.join("net/fib_trie")).ok()?;
    pick(&route, &trie)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
tailscale0\t0000400A\t00000000\t0001\t0\t0\t0\t0000C0FF\t0\t0\t0
";

    pub const FIB_TRIE: &str = "\
Main:
  +-- 0.0.0.0/0 3 0 5
     |-- 0.0.0.0
        /0 universe UNICAST
     +-- 100.64.0.0/10 2 0 2
        |-- 100.101.102.103
           /32 host LOCAL
     +-- 127.0.0.0/8 2 0 2
        |-- 127.0.0.1
           /32 host LOCAL
     +-- 192.168.1.0/24 2 0 2
        |-- 192.168.1.0
           /24 link UNICAST
        |-- 192.168.1.42
           /32 host LOCAL
Local:
  +-- 0.0.0.0/0 3 0 5
     |-- 192.168.1.42
        /32 host LOCAL
";

    #[test]
    fn picks_the_default_route_subnet() {
        assert_eq!(pick(ROUTE, FIB_TRIE), Some(Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(
            local_addrs(FIB_TRIE),
            [
                Ipv4Addr::new(100, 101, 102, 103),
                Ipv4Addr::new(127, 0, 0, 1),
                Ipv4Addr::new(192, 168, 1, 42)
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn without_default_route_or_addresses() {
        let no_default: String = ROUTE
            .lines()
            .filter(|l| !l.contains("0101A8C0"))
            .collect::<Vec<_>>()
            .join("\n");
        // A private address first, then any usable one.
        assert_eq!(
            pick(&no_default, FIB_TRIE),
            Some(Ipv4Addr::new(192, 168, 1, 42))
        );
        assert_eq!(
            pick(&no_default, "  |-- 100.101.102.103\n     /32 host LOCAL\n"),
            Some(Ipv4Addr::new(100, 101, 102, 103))
        );
        assert_eq!(
            pick(ROUTE, "Main:\n  |-- 127.0.0.1\n     /32 host LOCAL\n"),
            None
        );
        assert_eq!(pick("", ""), None);
    }
}

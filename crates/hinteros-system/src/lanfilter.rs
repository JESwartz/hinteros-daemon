//! The local network filter for apps without the `lan` permission.
//!
//! The session daemon starts those apps in a cgroup of its own (its unit has `Delegate=yes`)
//! and names it with `lan_filter.set`. The service checks that the cgroup is the caller's
//! ([`check_cgroup`]), keeps one per uid in `<run-dir>/lan-filter.json` ([`Filters`]) and starts
//! `hinteros-lan-filter.service`: a oneshot running `hinteros-system lan-filter-apply`
//! ([`apply_main`]), the only part with `CAP_NET_ADMIN`. It builds the nftables table
//! `inet hinteros_lan` ([`ruleset`]) and loads it with `nft -f -` in one transaction: when that
//! fails, the table stays what it was. The unit also runs when `/etc/resolv.conf` or the network
//! changes (the system repo's path unit and NetworkManager hook).
//!
//! From a filtered cgroup (and the cgroups below it), in the output hook: port 53 to the
//! nameservers of `/etc/resolv.conf` is allowed; TCP to a local address is reset and every other
//! packet to one is dropped (the sender gets `EPERM`). Local: the private, link-local, CGNAT,
//! loopback, multicast and broadcast ranges and the other IANA special-purpose ranges that are
//! not global ([`PRIVATE_V4`], [`PRIVATE_V6`]), the destinations of the box's on-link routes (a
//! home network on public addresses, IPv6 prefixes from the ISP), every local IPv4 network as
//! seen through NAT64 (`64:ff9b::/96`) and 6to4 (`2002::/16`) ([`embedded_v6`]), and the box's
//! own, broadcast and multicast addresses (`fib daddr type`). Not the documentation ranges
//! (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`): no network uses
//! them unless it is on-link, and the VM tests use them as the internet.
//!
//! nftables resolves a `socket cgroupv2` path to the cgroup's id when it loads the table, so a
//! cgroup removed and created again is not matched until the next load. Cgroups that are gone
//! are left out of the table (a session that ended without `lan_filter.clear`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Read, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// The nftables table (family `inet`).
pub const TABLE: &str = "hinteros_lan";

/// The oneshot unit that loads the table.
pub const APPLY_UNIT: &str = "hinteros-lan-filter.service";

/// The filtered cgroups, in the run dir.
pub const STATE_FILE: &str = "lan-filter.json";

/// The file format version.
pub const VERSION: u32 = 1;

/// Largest file read (state, resolv.conf, route tables).
const MAX_READ_BYTES: u64 = 1 << 20;

/// Most cgroup path parts below `user@<uid>.service`.
const MAX_DEPTH: usize = 16;

/// The IPv4 destinations that are local (RFC 1918, link-local, CGNAT, loopback, multicast,
/// broadcast, and the other non-global IANA special-purpose ranges: "this network", IETF
/// protocol assignments, benchmarking, reserved).
pub const PRIVATE_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (Ipv4Addr::new(255, 255, 255, 255), 32),
];

/// The IPv6 destinations that are local (unique local, link-local, multicast, loopback,
/// unspecified, IPv4-mapped, local-use NAT64, discard-only, benchmarking, site-local, SRv6).
pub const PRIVATE_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
];

/// The IPv6 addresses of local IPv4 networks: through the well-known NAT64 prefix
/// (`64:ff9b::/96`, RFC 6052) and 6to4 (`2002::/16`, RFC 3056). The rest of those prefixes is the
/// internet (an IPv6-only network reaches IPv4 sites through NAT64).
pub fn embedded_v6(nets: &[(Ipv4Addr, u8)]) -> Vec<(Ipv6Addr, u8)> {
    let nat64 = u128::from(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0));
    let six_to_four = u128::from(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0));
    nets.iter()
        .flat_map(|&(a, len)| {
            let a = u128::from(u32::from(a));
            let len = len.min(32);
            [
                (Ipv6Addr::from(nat64 | a), 96 + len),
                (Ipv6Addr::from(six_to_four | (a << 80)), 16 + len),
            ]
        })
        .collect()
}

/// `<run-dir>/lan-filter.json`: `{"version":1,"cgroups":{"<uid>":"user.slice/..."}}`, the
/// cgroup paths relative to the cgroup root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filters {
    pub version: u32,
    #[serde(default)]
    pub cgroups: BTreeMap<u32, String>,
}

impl Filters {
    /// Missing: none.
    pub fn load(path: &Path) -> io::Result<Self> {
        // The service's run dir: as root, no symlink and a regular file only (fsutil).
        let text = match crate::fsutil::read_limited(path, MAX_READ_BYTES) {
            Ok(t) => String::from_utf8(t).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let f: Self = serde_json::from_str(&text).map_err(io::Error::other)?;
        if f.version != VERSION {
            return Err(io::Error::other(format!(
                "unknown version {} in {}",
                f.version,
                path.display()
            )));
        }
        Ok(f)
    }

    /// Atomically, 0600.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = Self {
            version: VERSION,
            cgroups: self.cgroups.clone(),
        };
        let mut bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
        bytes.push(b'\n');
        crate::fsutil::write_atomic(path, &bytes, 0o600)
    }

    /// Leave out the cgroups that are gone (or no longer fit their uid).
    pub fn prune(&mut self, root: &Path) -> Vec<(u32, String)> {
        let mut gone = Vec::new();
        self.cgroups.retain(|uid, path| {
            let keep = check_cgroup(root, *uid, &format!("/{path}")).is_ok();
            if !keep {
                gone.push((*uid, path.clone()));
            }
            keep
        });
        gone
    }
}

fn read_limited(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_READ_BYTES)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Why `lan_filter.set` refuses a cgroup (`invalid_params {field: cgroup, reason}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupError {
    BadFormat,
    NotFound,
    NotOwned,
}

impl CgroupError {
    pub fn reason(self) -> &'static str {
        match self {
            CgroupError::BadFormat => "bad_format",
            CgroupError::NotFound => "not_found",
            CgroupError::NotOwned => "not_owned",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            CgroupError::BadFormat => "this is not a cgroup path below the user's service manager",
            CgroupError::NotFound => "no such cgroup",
            CgroupError::NotOwned => "not a cgroup of this user",
        }
    }
}

/// A cgroup path part: `[A-Za-z0-9._@:-]+`, not `.` or `..`.
fn is_part(p: &str) -> bool {
    !p.is_empty()
        && p != "."
        && p != ".."
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b':' | b'-'))
}

/// The shape alone: `/user.slice/user-<uid>.slice/user@<uid>.service/<part>[/<part>...]` with
/// `uid`'s number. Returns the path without its leading `/` (as nftables takes it).
pub fn check_format(uid: u32, path: &str) -> Result<String, CgroupError> {
    if path.len() > hinteros_api::system::MAX_CGROUP_PATH_BYTES {
        return Err(CgroupError::BadFormat);
    }
    let rel = path.strip_prefix('/').ok_or(CgroupError::BadFormat)?;
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() < 4 || parts.len() > 3 + MAX_DEPTH || !parts.iter().all(|p| is_part(p)) {
        return Err(CgroupError::BadFormat);
    }
    let number = |s: Option<&str>| {
        s.filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<u32>().ok())
    };
    let slice = number(
        parts[1]
            .strip_prefix("user-")
            .and_then(|s| s.strip_suffix(".slice")),
    );
    let manager = number(
        parts[2]
            .strip_prefix("user@")
            .and_then(|s| s.strip_suffix(".service")),
    );
    match (parts[0], slice, manager) {
        ("user.slice", Some(a), Some(b)) if a == b => {
            if a == uid {
                Ok(rel.to_owned())
            } else {
                Err(CgroupError::NotOwned)
            }
        }
        _ => Err(CgroupError::BadFormat),
    }
}

/// [`check_format`], then: a directory under the cgroup root `root` owned by `uid`.
pub fn check_cgroup(root: &Path, uid: u32, path: &str) -> Result<String, CgroupError> {
    let rel = check_format(uid, path)?;
    let meta = std::fs::symlink_metadata(root.join(&rel)).map_err(|_| CgroupError::NotFound)?;
    if !meta.is_dir() {
        return Err(CgroupError::NotFound);
    }
    if meta.uid() != uid {
        return Err(CgroupError::NotOwned);
    }
    Ok(rel)
}

/// The nameservers of a `resolv.conf` (scope ids dropped), in order, without repeats.
pub fn nameservers(resolv_conf: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in resolv_conf.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("nameserver") {
            continue;
        }
        let Some(addr) = words.next() else { continue };
        let addr = addr.split_once('%').map_or(addr, |(a, _)| a);
        if let Ok(ip) = addr.parse::<IpAddr>()
            && !out.contains(&ip)
        {
            out.push(ip);
        }
    }
    out
}

/// RTF_UP, RTF_GATEWAY and RTF_REJECT of `<linux/route.h>`.
const RTF_UP: u32 = 0x1;
const RTF_GATEWAY: u32 = 0x2;
const RTF_REJECT: u32 = 0x200;

/// The on-link IPv4 networks of `/proc/net/route` (up, no gateway, not `lo`, not the default
/// route).
pub fn onlink_v4(route: &str) -> Vec<(Ipv4Addr, u8)> {
    crate::servers::netinfo::routes(route)
        .into_iter()
        .filter_map(|r| {
            if r.iface == "lo" {
                return None;
            }
            let mask = u32::from(r.mask);
            let len = u8::try_from(mask.leading_ones()).ok()?;
            let usable = r.flags & RTF_UP != 0
                && r.flags & (RTF_GATEWAY | RTF_REJECT) == 0
                && len > 0
                && mask.count_ones() == u32::from(len);
            usable.then_some((r.dest, len))
        })
        .collect()
}

/// The on-link IPv6 networks of `/proc/net/ipv6_route` (up, no gateway, not `lo`, not the
/// default route).
pub fn onlink_v6(route: &str) -> Vec<(Ipv6Addr, u8)> {
    route
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[9] == "lo" {
                return None;
            }
            let dest = u128::from_str_radix(f[0], 16).ok()?;
            let len = u8::from_str_radix(f[1], 16).ok()?;
            let gateway = u128::from_str_radix(f[4], 16).ok()?;
            let flags = u32::from_str_radix(f[8], 16).ok()?;
            let usable = flags & RTF_UP != 0
                && flags & (RTF_GATEWAY | RTF_REJECT) == 0
                && gateway == 0
                && (1..=128).contains(&len);
            usable.then(|| (Ipv6Addr::from(dest), len))
        })
        .collect()
}

/// `nets` as network addresses (host bits cleared), without the ones another contains.
fn minimal<A: Copy + Ord>(
    nets: &[(A, u8)],
    bits: u8,
    to: fn(A) -> u128,
    from: fn(u128) -> A,
) -> Vec<(A, u8)> {
    let mask = |len: u8| -> u128 {
        if len == 0 {
            0
        } else {
            (!0u128) << (128 - u32::from(len))
        }
    };
    // Work on the top `bits` of a u128.
    let shift = 128 - u32::from(bits);
    let mut out: Vec<(u128, u8)> = nets
        .iter()
        .map(|&(a, len)| {
            let len = len.min(bits);
            ((to(a) << shift) & mask(len), len)
        })
        .collect();
    out.sort_by_key(|&(a, len)| (len, a));
    out.dedup();
    let mut kept: Vec<(u128, u8)> = Vec::new();
    for (a, len) in out {
        if !kept.iter().any(|&(k, klen)| a & mask(klen) == k) {
            kept.push((a, len));
        }
    }
    kept.sort();
    kept.into_iter()
        .map(|(a, len)| (from(a >> shift), len))
        .collect()
}

fn minimal_v4(nets: &[(Ipv4Addr, u8)]) -> Vec<(Ipv4Addr, u8)> {
    minimal(
        nets,
        32,
        |a| u128::from(u32::from(a)),
        |n| Ipv4Addr::from(n as u32),
    )
}

fn minimal_v6(nets: &[(Ipv6Addr, u8)]) -> Vec<(Ipv6Addr, u8)> {
    minimal(nets, 128, u128::from, Ipv6Addr::from)
}

/// What the table is made of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    /// (uid, cgroup path relative to the cgroup root).
    pub cgroups: Vec<(u32, String)>,
    pub nameservers: Vec<IpAddr>,
    /// The on-link networks.
    pub onlink_v4: Vec<(Ipv4Addr, u8)>,
    pub onlink_v6: Vec<(Ipv6Addr, u8)>,
}

/// The `nft -f` input: the table replaced in one transaction, or removed when no cgroup is
/// filtered.
pub fn ruleset(i: &Inputs) -> String {
    let mut s = format!("table inet {TABLE}\ndelete table inet {TABLE}\n");
    if i.cgroups.is_empty() {
        return s;
    }
    let elements = |items: Vec<String>| {
        if items.is_empty() {
            String::new()
        } else {
            format!(" elements = {{ {} }};", items.join(", "))
        }
    };
    let dns4: Vec<String> = i
        .nameservers
        .iter()
        .filter(|a| a.is_ipv4())
        .map(|a| a.to_string())
        .collect();
    let dns6: Vec<String> = i
        .nameservers
        .iter()
        .filter(|a| a.is_ipv6())
        .map(|a| a.to_string())
        .collect();
    let lan4: Vec<(Ipv4Addr, u8)> = PRIVATE_V4.iter().chain(&i.onlink_v4).copied().collect();
    let lan6: Vec<(Ipv6Addr, u8)> = PRIVATE_V6
        .iter()
        .chain(&i.onlink_v6)
        .copied()
        .chain(embedded_v6(&lan4))
        .collect();
    let lan4: Vec<String> = minimal_v4(&lan4)
        .into_iter()
        .map(|(a, l)| format!("{a}/{l}"))
        .collect();
    let lan6: Vec<String> = minimal_v6(&lan6)
        .into_iter()
        .map(|(a, l)| format!("{a}/{l}"))
        .collect();
    let _ = writeln!(s, "table inet {TABLE} {{");
    let _ = writeln!(s, "\tset dns4 {{ type ipv4_addr;{} }}", elements(dns4));
    let _ = writeln!(s, "\tset dns6 {{ type ipv6_addr;{} }}", elements(dns6));
    let _ = writeln!(
        s,
        "\tset lan4 {{ type ipv4_addr; flags interval;{} }}",
        elements(lan4)
    );
    let _ = writeln!(
        s,
        "\tset lan6 {{ type ipv6_addr; flags interval;{} }}",
        elements(lan6)
    );
    s.push_str("\tchain output {\n\t\ttype filter hook output priority filter; policy accept;\n");
    for (uid, path) in &i.cgroups {
        let level = path.split('/').count();
        let _ = writeln!(
            s,
            "\t\tsocket cgroupv2 level {level} \"{path}\" jump nolan comment \"uid {uid}\""
        );
    }
    s.push_str("\t}\n");
    s.push_str(concat!(
        "\tchain nolan {\n",
        "\t\tmeta l4proto { tcp, udp } th dport 53 ip daddr @dns4 accept\n",
        "\t\tmeta l4proto { tcp, udp } th dport 53 ip6 daddr @dns6 accept\n",
        "\t\tip daddr @lan4 jump refuse\n",
        "\t\tip6 daddr @lan6 jump refuse\n",
        "\t\tfib daddr type { local, broadcast, multicast } jump refuse\n",
        "\t}\n",
        "\tchain refuse {\n",
        "\t\tmeta l4proto tcp reject with tcp reset\n",
        "\t\tdrop\n",
        "\t}\n",
        "}\n",
    ));
    s
}

/// Where `lan-filter-apply` looks.
#[derive(Debug, Clone)]
pub struct ApplyPaths {
    pub run_dir: PathBuf,
    pub cgroup_root: PathBuf,
    pub resolv_conf: PathBuf,
    pub proc_dir: PathBuf,
    pub nft: PathBuf,
}

/// The inputs from the files: the cgroups that are still there, the nameservers, the on-link
/// routes (a file that cannot be read gives none; the state file must be readable).
pub fn gather(p: &ApplyPaths) -> io::Result<Inputs> {
    let mut filters = Filters::load(&p.run_dir.join(STATE_FILE))?;
    for (uid, path) in filters.prune(&p.cgroup_root) {
        info!(uid, cgroup = %path, "cgroup gone or not the user's: not filtered");
    }
    let read = |path: &Path| {
        read_limited(path).unwrap_or_else(|e| {
            warn!(file = %path.display(), "{e}");
            String::new()
        })
    };
    Ok(Inputs {
        cgroups: filters.cgroups.into_iter().collect(),
        nameservers: nameservers(&read(&p.resolv_conf)),
        onlink_v4: onlink_v4(&read(&p.proc_dir.join("net/route"))),
        onlink_v6: onlink_v6(&read(&p.proc_dir.join("net/ipv6_route"))),
    })
}

/// Load `rules` with `nft -f -`.
pub fn load(nft: &Path, rules: &str) -> io::Result<()> {
    let mut child = Command::new(nft)
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(rules.as_bytes())?;
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "nft: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// `hinteros-system lan-filter-apply` (the oneshot unit): build the table and load it.
pub fn apply_main(p: &ApplyPaths) -> ExitCode {
    let result = gather(p).and_then(|inputs| {
        load(&p.nft, &ruleset(&inputs))?;
        Ok(inputs)
    });
    match result {
        Ok(i) => {
            info!(
                cgroups = i.cgroups.len(),
                nameservers = ?i.nameservers,
                onlink = i.onlink_v4.len() + i.onlink_v6.len(),
                "local network filter loaded"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("local network filter not loaded: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/hinteros-session.service/nolan";

    #[test]
    fn cgroup_format() {
        assert_eq!(check_format(1000, SESSION).unwrap(), &SESSION[1..]);
        assert_eq!(check_format(1001, SESSION), Err(CgroupError::NotOwned));
        for bad in [
            "",
            "/",
            "user.slice/user-1000.slice/user@1000.service/x",
            "/user.slice/user-1000.slice/user@1000.service",
            "/user.slice/user-1000.slice/user@1000.service/",
            "/user.slice/user-1000.slice/user@1001.service/x",
            "/user.slice/user-.slice/user@.service/x",
            "/user.slice/user-1000.slice/user@1000.service/../../user@1001.service/x",
            "/user.slice/user-1000.slice/user@1000.service/./x",
            "/user.slice/user-1000.slice/user@1000.service//x",
            "/user.slice/user-1000.slice/user@1000.service/x\"y",
            "/user.slice/user-1000.slice/user@1000.service/x y",
            "/user.slice/user-1000.slice/user@1000.service/x\\x2d",
            "/system.slice/user-1000.slice/user@1000.service/x",
            "/user.slice/user-1000.slice/session-2.scope/x",
            "/user.slice/user-+1000.slice/user@+1000.service/x",
        ] {
            assert_eq!(
                check_format(1000, bad),
                Err(CgroupError::BadFormat),
                "{bad}"
            );
        }
        let deep = format!("{SESSION}{}", "/a".repeat(MAX_DEPTH));
        assert_eq!(check_format(1000, &deep), Err(CgroupError::BadFormat));
        let long = format!("{SESSION}/{}", "a".repeat(600));
        assert_eq!(check_format(1000, &long), Err(CgroupError::BadFormat));
    }

    #[test]
    fn cgroup_on_disk() {
        let t = tempfile::tempdir().unwrap();
        let me = rustix::process::getuid().as_raw();
        let path = SESSION.replace("1000", &me.to_string());
        assert_eq!(
            check_cgroup(t.path(), me, &path),
            Err(CgroupError::NotFound)
        );
        std::fs::create_dir_all(t.path().join(&path[1..])).unwrap();
        assert_eq!(check_cgroup(t.path(), me, &path).unwrap(), &path[1..]);
        // A file is not a cgroup.
        let file = format!("{path}/cgroup.procs");
        std::fs::write(t.path().join(&file[1..]), "").unwrap();
        assert_eq!(
            check_cgroup(t.path(), me, &file),
            Err(CgroupError::NotFound)
        );
        // Pruned when gone.
        let mut f = Filters::default();
        f.cgroups.insert(me, path[1..].to_owned());
        f.cgroups.insert(
            me + 1,
            format!("user.slice/user-{0}.slice/user@{0}.service/x", me + 1),
        );
        let gone = f.prune(t.path());
        assert_eq!(gone.len(), 1);
        assert_eq!(f.cgroups.keys().copied().collect::<Vec<_>>(), [me]);
    }

    #[test]
    fn state_file() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join(STATE_FILE);
        assert_eq!(Filters::load(&path).unwrap(), Filters::default());
        let mut f = Filters::default();
        f.cgroups.insert(1000, SESSION[1..].to_owned());
        f.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\"version\":1,\"cgroups\":{\"1000\":"),
            "{text}"
        );
        let back = Filters::load(&path).unwrap();
        assert_eq!(back.cgroups, f.cgroups);
        std::fs::write(&path, r#"{"version":2,"cgroups":{}}"#).unwrap();
        assert!(Filters::load(&path).is_err());
        std::fs::write(&path, "{").unwrap();
        assert!(Filters::load(&path).is_err());
    }

    #[test]
    fn resolv_conf_nameservers() {
        let text = "# Generated by NetworkManager\nsearch home\nnameserver 192.168.1.1\n\
                    nameserver fe80::1%enp1s0\nnameserver 192.168.1.1\nnameserver x\n\
                    ;nameserver 9.9.9.9\noptions edns0\n  nameserver 127.0.0.53\n";
        assert_eq!(
            nameservers(text),
            [
                "192.168.1.1".parse::<IpAddr>().unwrap(),
                "fe80::1".parse().unwrap(),
                "127.0.0.53".parse().unwrap()
            ]
        );
        assert!(nameservers("").is_empty());
    }

    #[test]
    fn onlink_routes() {
        // A default route via 192.168.1.1, the LAN, a public on-link /24, a reject route, lo.
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            eth1\t007100CB\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
            eth1\t0000000A\t00000000\t0201\t0\t0\t100\t000000FF\t0\t0\t0\n\
            lo\t0000007F\t00000000\t0001\t0\t0\t0\t000000FF\t0\t0\t0\n";
        // The test data is what a little-endian kernel prints.
        #[cfg(target_endian = "little")]
        assert_eq!(
            onlink_v4(route),
            [
                (Ipv4Addr::new(192, 168, 1, 0), 24),
                (Ipv4Addr::new(203, 0, 113, 0), 24),
            ]
        );
        let route6 = "\
            00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003     eth0\n\
            20010db8000100020000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0\n\
            fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0\n\
            20010db8000900000000000000000000 30 00000000000000000000000000000000 00 20010db8000100020000000000000001 00000400 00000001 00000000 00000003     eth0\n\
            00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n\
            00000000000000000000000000000001 80 00000000000000000000000000000000 00 00000000000000000000000000000000 00000000 00000002 00000000 80200001       lo\n";
        assert_eq!(
            onlink_v6(route6),
            [
                ("2001:db8:1:2::".parse().unwrap(), 64),
                ("fe80::".parse().unwrap(), 64)
            ]
        );
        assert!(onlink_v4("").is_empty() && onlink_v6("garbage line").is_empty());
    }

    #[test]
    fn nested_networks_are_merged() {
        let v4 = minimal_v4(&[
            (Ipv4Addr::new(192, 168, 1, 7), 24),
            (Ipv4Addr::new(192, 168, 0, 0), 16),
            (Ipv4Addr::new(203, 0, 113, 0), 24),
            (Ipv4Addr::new(203, 0, 113, 0), 24),
            (Ipv4Addr::new(10, 1, 0, 0), 16),
            (Ipv4Addr::new(10, 2, 0, 0), 16),
        ]);
        assert_eq!(
            v4,
            [
                (Ipv4Addr::new(10, 1, 0, 0), 16),
                (Ipv4Addr::new(10, 2, 0, 0), 16),
                (Ipv4Addr::new(192, 168, 0, 0), 16),
                (Ipv4Addr::new(203, 0, 113, 0), 24),
            ]
        );
        let v6 = minimal_v6(&[
            ("fe80::".parse().unwrap(), 64),
            ("fe80::".parse().unwrap(), 10),
            ("2001:db8:1:2::5".parse().unwrap(), 64),
        ]);
        assert_eq!(
            v6,
            [
                ("2001:db8:1:2::".parse().unwrap(), 64),
                ("fe80::".parse().unwrap(), 10)
            ]
        );
    }

    #[test]
    fn nat64_and_6to4_of_local_networks() {
        let v6 = embedded_v6(&[
            (Ipv4Addr::new(192, 168, 1, 0), 24),
            (Ipv4Addr::new(255, 255, 255, 255), 32),
        ]);
        assert_eq!(
            v6,
            [
                ("64:ff9b::c0a8:100".parse().unwrap(), 120),
                ("2002:c0a8:100::".parse().unwrap(), 40),
                ("64:ff9b::ffff:ffff".parse().unwrap(), 128),
                ("2002:ffff:ffff::".parse().unwrap(), 48),
            ]
        );
    }

    #[test]
    fn the_table() {
        // No cgroup: the table is removed.
        let none = ruleset(&Inputs::default());
        assert_eq!(
            none,
            "table inet hinteros_lan\ndelete table inet hinteros_lan\n"
        );
        let i = Inputs {
            cgroups: vec![
                (1000, SESSION[1..].to_owned()),
                (
                    1001,
                    "user.slice/user-1001.slice/user@1001.service/a/nolan".into(),
                ),
            ],
            nameservers: vec!["192.168.1.1".parse().unwrap(), "fe80::1".parse().unwrap()],
            onlink_v4: vec![
                (Ipv4Addr::new(203, 0, 113, 0), 24),
                (Ipv4Addr::new(192, 168, 1, 0), 24),
            ],
            onlink_v6: vec![("2001:db8:1:2::".parse().unwrap(), 64)],
        };
        let r = ruleset(&i);
        assert!(r.starts_with("table inet hinteros_lan\ndelete table inet hinteros_lan\ntable inet hinteros_lan {\n"), "{r}");
        let line = |want: &str| {
            assert!(
                r.lines().any(|l| l.trim() == want),
                "missing {want:?} in\n{r}"
            );
        };
        line("set dns4 { type ipv4_addr; elements = { 192.168.1.1 }; }");
        line("set dns6 { type ipv6_addr; elements = { fe80::1 }; }");
        line(
            "set lan4 { type ipv4_addr; flags interval; elements = { 0.0.0.0/8, 10.0.0.0/8, \
             100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, \
             192.168.0.0/16, 198.18.0.0/15, 203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4 }; }",
        );
        let lan6 = r
            .lines()
            .find(|l| l.trim().starts_with("set lan6 "))
            .unwrap();
        for want in [
            " ::/128,",
            " ::1/128,",
            " ::ffff:0.0.0.0/96,",
            " 64:ff9b::a00:0/104,",
            " 64:ff9b::c0a8:0/112,",
            " 64:ff9b::cb00:7100/120,",
            " 64:ff9b::7f00:0/104,",
            " 64:ff9b:1::/48,",
            " 100::/64,",
            " 2001:2::/48,",
            " 2001:db8:1:2::/64,",
            " 2002:a00::/24,",
            " 2002:c0a8::/32,",
            " 2002:cb00:7100::/40,",
            " 5f00::/16,",
            " fc00::/7,",
            " fe80::/10,",
            " fec0::/10,",
            " ff00::/8 ",
        ] {
            assert!(lan6.contains(want), "{want:?} not in {lan6}");
        }
        // NAT64 and 6to4 to the internet stay open.
        assert!(
            !lan6.contains(" 64:ff9b::/96") && !lan6.contains(" 2002::/16"),
            "{lan6}"
        );
        line("type filter hook output priority filter; policy accept;");
        line(&format!(
            "socket cgroupv2 level 6 \"{}\" jump nolan comment \"uid 1000\"",
            &SESSION[1..]
        ));
        line(
            "socket cgroupv2 level 5 \"user.slice/user-1001.slice/user@1001.service/a/nolan\" jump nolan comment \"uid 1001\"",
        );
        // DNS first, then the refusals.
        let at = |want: &str| r.find(want).unwrap();
        assert!(at("th dport 53 ip daddr @dns4 accept") < at("ip daddr @lan4 jump refuse"));
        assert!(at("th dport 53 ip6 daddr @dns6 accept") < at("ip6 daddr @lan6 jump refuse"));
        line("fib daddr type { local, broadcast, multicast } jump refuse");
        line("meta l4proto tcp reject with tcp reset");
        line("drop");
        // No nameserver: empty sets, no DNS to the network at all.
        let r = ruleset(&Inputs {
            nameservers: vec![],
            ..i
        });
        assert!(r.contains("set dns4 { type ipv4_addr; }"), "{r}");
        assert!(r.contains("set dns6 { type ipv6_addr; }"), "{r}");
    }

    #[test]
    fn gather_and_load() {
        let t = tempfile::tempdir().unwrap();
        let me = rustix::process::getuid().as_raw();
        let root = t.path().join("cgroup");
        let rel = format!("user.slice/user-{me}.slice/user@{me}.service/s/nolan");
        std::fs::create_dir_all(root.join(&rel)).unwrap();
        let mut f = Filters::default();
        f.cgroups.insert(me, rel.clone());
        f.cgroups.insert(
            me + 1,
            format!("user.slice/user-{0}.slice/user@{0}.service/gone", me + 1),
        );
        f.save(&t.path().join(STATE_FILE)).unwrap();
        std::fs::write(t.path().join("resolv.conf"), "nameserver 192.168.1.1\n").unwrap();
        let log = t.path().join("nft.log");
        let nft = t.path().join("nft");
        std::fs::write(
            &nft,
            format!(
                "#!/bin/sh\necho \"$@\" > {0}.args\ncat > {0}\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&nft, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = ApplyPaths {
            run_dir: t.path().into(),
            cgroup_root: root,
            resolv_conf: t.path().join("resolv.conf"),
            proc_dir: t.path().join("no-proc"),
            nft: nft.clone(),
        };
        let i = gather(&p).unwrap();
        assert_eq!(i.cgroups, [(me, rel.clone())]);
        assert_eq!(i.nameservers, ["192.168.1.1".parse::<IpAddr>().unwrap()]);
        assert!(i.onlink_v4.is_empty() && i.onlink_v6.is_empty());
        assert_eq!(apply_main(&p), ExitCode::SUCCESS);
        assert_eq!(
            std::fs::read_to_string(format!("{}.args", log.display())).unwrap(),
            "-f -\n"
        );
        assert!(std::fs::read_to_string(&log).unwrap().contains(&rel));
        // nft fails: so does the unit.
        std::fs::write(
            &nft,
            "#!/bin/sh\ncat >/dev/null\necho 'Error: no' >&2\nexit 1\n",
        )
        .unwrap();
        assert_eq!(apply_main(&p), ExitCode::FAILURE);
        let e = load(&nft, "x").unwrap_err();
        assert!(e.to_string().contains("Error: no"), "{e}");
        // An unreadable state file fails too (never a table without the cgroups).
        std::fs::write(t.path().join(STATE_FILE), "{").unwrap();
        assert!(gather(&p).is_err());
    }
}

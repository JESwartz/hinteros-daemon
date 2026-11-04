//! The system call filter of every sandboxed app: a classic BPF program that bwrap loads
//! (`--seccomp <fd>`) just before it starts the app.
//!
//! A denylist in the style of Flatpak's: what an app never needs and what has a history of
//! kernel bugs fails with `EPERM` (`ENOSYS` where a fallback must kick in, `EAFNOSUPPORT` for
//! socket families). Everything else is allowed. User namespaces, `mount`, `pivot_root`,
//! `chroot`, `unshare` and `clone` stay allowed: Firefox, Chromium (Electron, Qt WebEngine) and
//! Plex's FHS bwrap make their own sandboxes inside ours (unlike Flatpak, which blocks them).
//!
//! - Only the native ABI (x86-64): another architecture (i386 `int 0x80`) kills the process,
//!   x32 system calls fail with `ENOSYS`. No sandboxed app is 32-bit (Steam is not sandboxed).
//! - [`DENY`]: kernel keyrings, io_uring, BPF, perf, userfaultfd, NUMA policy, kernel
//!   modules, kexec, reboot, swap, clocks, the kernel log, quotas, accounting, file handles,
//!   `vhangup`, `iopl`/`ioperm`, obsolete calls; the new mount API (`ENOSYS`, so that tools fall
//!   back to `mount`; it is reachable in a nested user namespace and has had bugs).
//! - `ioctl` `TIOCSTI` and `TIOCLINUX` (typing into the terminal).
//! - `personality`: only [`PERSONALITIES`] (Linux, 32-bit uname, the query).
//! - `socket`: only [`SOCKET_FAMILIES`] (Unix, IPv4, IPv6, netlink); no `AF_PACKET`, `AF_ALG`,
//!   `AF_VSOCK` and the like, also not in a nested network namespace. Netlink only for
//!   [`NETLINK_PROTOCOLS`] (route, uevent): no nf_tables, XFRM, generic netlink.
//!
//! [`program`] is pure. [`pipe`] gives bwrap the program on an inherited file descriptor. When
//! the filter cannot be made, the launch fails; when bwrap cannot load it, bwrap exits before
//! it starts the app.

use std::io::{self, Write};
use std::os::fd::OwnedFd;

use nix::libc;

/// What a filtered system call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Errno(i32),
    KillProcess,
}

impl Verdict {
    fn ret(self) -> u32 {
        match self {
            Verdict::Allow => SECCOMP_RET_ALLOW,
            Verdict::Errno(e) => SECCOMP_RET_ERRNO | (e as u32 & SECCOMP_RET_DATA),
            Verdict::KillProcess => SECCOMP_RET_KILL_PROCESS,
        }
    }
}

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

/// `AUDIT_ARCH_X86_64`.
#[cfg(target_arch = "x86_64")]
const NATIVE_ARCH: u32 = 0xc000_003e;
/// x32 system call numbers are the x86-64 ones with this bit.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// `open_tree_attr` (Linux 6.15; not in libc yet).
#[cfg(target_arch = "x86_64")]
const SYS_OPEN_TREE_ATTR: i64 = 467;

/// System calls that fail, and their errno.
#[cfg(target_arch = "x86_64")]
pub const DENY: &[(i64, i32)] = &[
    // Kernel keyrings.
    (libc::SYS_add_key, libc::EPERM),
    (libc::SYS_keyctl, libc::EPERM),
    (libc::SYS_request_key, libc::EPERM),
    // io_uring (also off box-wide: kernel.io_uring_disabled).
    (libc::SYS_io_uring_setup, libc::EPERM),
    (libc::SYS_io_uring_enter, libc::EPERM),
    (libc::SYS_io_uring_register, libc::EPERM),
    // BPF, profiling, userfaultfd.
    (libc::SYS_bpf, libc::EPERM),
    (libc::SYS_perf_event_open, libc::EPERM),
    (libc::SYS_userfaultfd, libc::EPERM),
    // NUMA memory policy (Flatpak: "scary VM/NUMA ops").
    (libc::SYS_mbind, libc::EPERM),
    (libc::SYS_get_mempolicy, libc::EPERM),
    (libc::SYS_set_mempolicy, libc::EPERM),
    (libc::SYS_set_mempolicy_home_node, libc::EPERM),
    (libc::SYS_migrate_pages, libc::EPERM),
    (libc::SYS_move_pages, libc::EPERM),
    // The kernel and the machine.
    (libc::SYS_init_module, libc::EPERM),
    (libc::SYS_finit_module, libc::EPERM),
    (libc::SYS_delete_module, libc::EPERM),
    (libc::SYS_kexec_load, libc::EPERM),
    (libc::SYS_kexec_file_load, libc::EPERM),
    (libc::SYS_reboot, libc::EPERM),
    (libc::SYS_swapon, libc::EPERM),
    (libc::SYS_swapoff, libc::EPERM),
    (libc::SYS_acct, libc::EPERM),
    (libc::SYS_syslog, libc::EPERM),
    (libc::SYS_quotactl, libc::EPERM),
    (libc::SYS_quotactl_fd, libc::EPERM),
    (libc::SYS_iopl, libc::EPERM),
    (libc::SYS_ioperm, libc::EPERM),
    (libc::SYS_vhangup, libc::EPERM),
    // Clocks.
    (libc::SYS_settimeofday, libc::EPERM),
    (libc::SYS_clock_settime, libc::EPERM),
    (libc::SYS_adjtimex, libc::EPERM),
    (libc::SYS_clock_adjtime, libc::EPERM),
    // Files by handle (bypasses paths).
    (libc::SYS_open_by_handle_at, libc::EPERM),
    // Obsolete.
    (libc::SYS_uselib, libc::EPERM),
    (libc::SYS_lookup_dcookie, libc::EPERM),
    (libc::SYS_nfsservctl, libc::EPERM),
    (libc::SYS__sysctl, libc::EPERM),
    // The new mount API: ENOSYS, so that tools fall back to mount(2).
    (libc::SYS_open_tree, libc::ENOSYS),
    (SYS_OPEN_TREE_ATTR, libc::ENOSYS),
    (libc::SYS_move_mount, libc::ENOSYS),
    (libc::SYS_fsopen, libc::ENOSYS),
    (libc::SYS_fsconfig, libc::ENOSYS),
    (libc::SYS_fsmount, libc::ENOSYS),
    (libc::SYS_fspick, libc::ENOSYS),
    (libc::SYS_mount_setattr, libc::ENOSYS),
];

/// `ioctl` requests that fail (`EPERM`): typing into the terminal, console copy and paste.
pub const IOCTLS: &[u32] = &[libc::TIOCSTI as u32, libc::TIOCLINUX as u32];

/// The only `personality` values allowed (`PER_LINUX`, `PER_LINUX32`, `UNAME26` with either,
/// the query); others fail (`EPERM`): no `ADDR_NO_RANDOMIZE`, `READ_IMPLIES_EXEC`,
/// `MMAP_PAGE_ZERO`, other ABIs.
pub const PERSONALITIES: &[u32] = &[0x0, 0x8, 0x2_0000, 0x2_0008, 0xffff_ffff];

/// The only socket families allowed; others fail (`EAFNOSUPPORT`).
pub const SOCKET_FAMILIES: &[u32] = &[
    libc::AF_UNSPEC as u32,
    libc::AF_UNIX as u32,
    libc::AF_INET as u32,
    libc::AF_INET6 as u32,
    libc::AF_NETLINK as u32,
];

/// The only netlink protocols allowed; others fail (`EPROTONOSUPPORT`). Apps read interfaces
/// and addresses (`NETLINK_ROUTE`) and watch devices (`NETLINK_KOBJECT_UEVENT`). The rest, and
/// above all `NETLINK_NETFILTER` (nf_tables, with `CAP_NET_ADMIN` in a nested user and network
/// namespace), has a history of kernel bugs.
pub const NETLINK_PROTOCOLS: &[u32] = &[
    libc::NETLINK_ROUTE as u32,
    libc::NETLINK_KOBJECT_UEVENT as u32,
];

/// One classic BPF instruction (`struct sock_filter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

// struct seccomp_data: nr, arch, instruction_pointer, args[6] (little-endian: low word first).
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
fn off_arg_low(i: u32) -> u32 {
    16 + 8 * i
}

fn ld(off: u32) -> Insn {
    Insn {
        code: LD_W_ABS,
        jt: 0,
        jf: 0,
        k: off,
    }
}
fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: JEQ_K,
        jt,
        jf,
        k,
    }
}
fn ret(v: Verdict) -> Insn {
    Insn {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: v.ret(),
    }
}

/// For system call `nr`: check the low 32 bits of argument `arg` (all of these take an `int`
/// or `unsigned int` there, so the kernel ignores the high bits too). With `listed_fail`, the
/// listed values get `errno`; else only the listed values are allowed.
fn arg_rule(out: &mut Vec<Insn>, nr: i64, arg: u32, values: &[u32], listed_fail: bool, errno: i32) {
    let n = u8::try_from(values.len()).expect("few values");
    // ld; one jeq per value; two returns.
    out.push(jeq(nr as u32, 0, n + 3));
    out.push(ld(off_arg_low(arg)));
    for (i, v) in values.iter().enumerate() {
        // To the second return.
        out.push(jeq(*v, n - i as u8, 0));
    }
    let (first, second) = if listed_fail {
        (Verdict::Allow, Verdict::Errno(errno))
    } else {
        (Verdict::Errno(errno), Verdict::Allow)
    };
    out.push(ret(first));
    out.push(ret(second));
}

/// The filter's instructions.
#[cfg(target_arch = "x86_64")]
pub fn instructions() -> Vec<Insn> {
    let mut out = vec![
        ld(OFF_ARCH),
        jeq(NATIVE_ARCH, 1, 0),
        ret(Verdict::KillProcess),
        ld(OFF_NR),
        Insn {
            code: JGE_K,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        ret(Verdict::Errno(libc::ENOSYS)),
    ];
    for &(nr, errno) in DENY {
        out.push(jeq(nr as u32, 0, 1));
        out.push(ret(Verdict::Errno(errno)));
    }
    arg_rule(&mut out, libc::SYS_ioctl, 1, IOCTLS, true, libc::EPERM);
    arg_rule(
        &mut out,
        libc::SYS_personality,
        0,
        PERSONALITIES,
        false,
        libc::EPERM,
    );
    socket_rule(&mut out);
    out.push(ret(Verdict::Allow));
    out
}

/// `socket`: only [`SOCKET_FAMILIES`] (else `EAFNOSUPPORT`), and for `AF_NETLINK` only
/// [`NETLINK_PROTOCOLS`] (else `EPROTONOSUPPORT`).
fn socket_rule(out: &mut Vec<Insn>) {
    let np = u8::try_from(NETLINK_PROTOCOLS.len()).expect("few values");
    let nf = u8::try_from(SOCKET_FAMILIES.len()).expect("few values");
    // The netlink part: ld; one jeq per protocol; two returns.
    let netlink = np + 3;
    // The family part: one jeq per family; two returns.
    let families = nf + 2;
    out.push(jeq(libc::SYS_socket as u32, 0, 2 + netlink + families));
    out.push(ld(off_arg_low(0)));
    out.push(jeq(libc::AF_NETLINK as u32, 0, netlink));
    out.push(ld(off_arg_low(2)));
    for (i, p) in NETLINK_PROTOCOLS.iter().enumerate() {
        out.push(jeq(*p, np - i as u8, 0));
    }
    out.push(ret(Verdict::Errno(libc::EPROTONOSUPPORT)));
    out.push(ret(Verdict::Allow));
    for (i, f) in SOCKET_FAMILIES.iter().enumerate() {
        out.push(jeq(*f, nf - i as u8, 0));
    }
    out.push(ret(Verdict::Errno(libc::EAFNOSUPPORT)));
    out.push(ret(Verdict::Allow));
}

/// The filter as bwrap reads it: `struct sock_filter` array, native byte order.
#[cfg(target_arch = "x86_64")]
pub fn program() -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for i in instructions() {
        out.extend(i.code.to_ne_bytes());
        out.push(i.jt);
        out.push(i.jf);
        out.extend(i.k.to_ne_bytes());
    }
    Ok(out)
}

/// No filter for other architectures: sandboxed launches fail.
#[cfg(not(target_arch = "x86_64"))]
pub fn program() -> io::Result<Vec<u8>> {
    Err(io::Error::other("no seccomp filter for this architecture"))
}

/// The read end of a pipe holding [`program`], inherited by children (no close-on-exec): pass
/// its number to bwrap (`--seccomp <fd>`, bwrap reads it to the end and closes it), and drop
/// it after the spawn.
pub fn pipe() -> io::Result<OwnedFd> {
    let program = program()?;
    let (read, mut write) = io::pipe()?;
    // Much less than a pipe's buffer: this does not block.
    write.write_all(&program)?;
    drop(write);
    let read = OwnedFd::from(read);
    rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::empty())?;
    Ok(read)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    /// Run the program on one system call (a classic BPF interpreter, just the instructions
    /// used here).
    fn run(prog: &[Insn], arch: u32, nr: u32, args: [u64; 6]) -> Verdict {
        let mut data = Vec::new();
        data.extend(nr.to_le_bytes());
        data.extend(arch.to_le_bytes());
        data.extend(0u64.to_le_bytes());
        for a in args {
            data.extend(a.to_le_bytes());
        }
        let word = |off: u32| {
            let o = off as usize;
            u32::from_le_bytes(data[o..o + 4].try_into().unwrap())
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let i = prog[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => acc = word(i.k),
                JEQ_K | JGE_K => {
                    let hit = if i.code == JEQ_K {
                        acc == i.k
                    } else {
                        acc >= i.k
                    };
                    pc += usize::from(if hit { i.jt } else { i.jf });
                }
                RET_K => {
                    return match i.k {
                        SECCOMP_RET_ALLOW => Verdict::Allow,
                        SECCOMP_RET_KILL_PROCESS => Verdict::KillProcess,
                        k if k & !SECCOMP_RET_DATA == SECCOMP_RET_ERRNO => {
                            Verdict::Errno((k & SECCOMP_RET_DATA) as i32)
                        }
                        k => panic!("return {k:#x}"),
                    };
                }
                c => panic!("instruction {c:#x}"),
            }
        }
    }

    fn call(nr: i64, args: [u64; 6]) -> Verdict {
        run(&instructions(), NATIVE_ARCH, nr as u32, args)
    }

    #[test]
    fn denied_calls_fail_with_their_errno() {
        for &(nr, errno) in DENY {
            assert_eq!(call(nr, [0; 6]), Verdict::Errno(errno), "{nr}");
        }
        assert_eq!(call(libc::SYS_keyctl, [0; 6]), Verdict::Errno(libc::EPERM));
        assert_eq!(
            call(libc::SYS_io_uring_setup, [1, 0, 0, 0, 0, 0]),
            Verdict::Errno(libc::EPERM)
        );
        assert_eq!(call(libc::SYS_fsopen, [0; 6]), Verdict::Errno(libc::ENOSYS));
    }

    #[test]
    fn nested_sandboxes_and_ordinary_calls_are_allowed() {
        let newuser = libc::CLONE_NEWUSER as u64;
        for (nr, args) in [
            (
                libc::SYS_unshare,
                [newuser | libc::CLONE_NEWNS as u64, 0, 0, 0, 0, 0],
            ),
            (libc::SYS_clone, [newuser, 0, 0, 0, 0, 0]),
            (libc::SYS_clone3, [0; 6]),
            (libc::SYS_setns, [0; 6]),
            (libc::SYS_mount, [0; 6]),
            (libc::SYS_umount2, [0; 6]),
            (libc::SYS_pivot_root, [0; 6]),
            (libc::SYS_chroot, [0; 6]),
            (libc::SYS_seccomp, [0; 6]),
            (libc::SYS_prctl, [0; 6]),
            (libc::SYS_ptrace, [0; 6]),
            (libc::SYS_kcmp, [0; 6]),
            (libc::SYS_name_to_handle_at, [0; 6]),
            (libc::SYS_read, [0; 6]),
            (libc::SYS_openat, [0; 6]),
            (libc::SYS_execve, [0; 6]),
            (libc::SYS_exit_group, [0; 6]),
            (libc::SYS_memfd_create, [0; 6]),
        ] {
            assert_eq!(call(nr, args), Verdict::Allow, "{nr}");
        }
    }

    #[test]
    fn ioctl_terminal_injection_fails_only() {
        let ioctl = |req: u64| call(libc::SYS_ioctl, [0, req, 0, 0, 0, 0]);
        assert_eq!(ioctl(libc::TIOCSTI), Verdict::Errno(libc::EPERM));
        assert_eq!(ioctl(libc::TIOCLINUX), Verdict::Errno(libc::EPERM));
        // The kernel ignores the high bits of the request: so do we.
        assert_eq!(
            ioctl(0xdead_0000_0000_0000 | libc::TIOCSTI),
            Verdict::Errno(libc::EPERM)
        );
        for req in [
            libc::TIOCGWINSZ,
            libc::FIONREAD,
            0xc018_6445, /* a DRM ioctl */
        ] {
            assert_eq!(ioctl(req), Verdict::Allow, "{req:#x}");
        }
    }

    #[test]
    fn personality_and_socket_families() {
        let pers = |p: u64| call(libc::SYS_personality, [p, 0, 0, 0, 0, 0]);
        for p in [0, 8, 0x2_0000, 0x2_0008, 0xffff_ffff] {
            assert_eq!(pers(p), Verdict::Allow, "{p:#x}");
        }
        // ADDR_NO_RANDOMIZE, READ_IMPLIES_EXEC, MMAP_PAGE_ZERO, PER_BSD.
        for p in [0x4_0000, 0x40_0000, 0x10_0000, 0x6] {
            assert_eq!(pers(p), Verdict::Errno(libc::EPERM), "{p:#x}");
        }
        let socket = |f: i32| call(libc::SYS_socket, [f as u64, 0, 0, 0, 0, 0]);
        for f in [
            libc::AF_UNIX,
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_NETLINK,
        ] {
            assert_eq!(socket(f), Verdict::Allow, "{f}");
        }
        for f in [
            libc::AF_PACKET,
            libc::AF_ALG,
            libc::AF_VSOCK,
            libc::AF_BLUETOOTH,
            libc::AF_KEY,
            libc::AF_TIPC,
            libc::AF_RDS,
        ] {
            assert_eq!(socket(f), Verdict::Errno(libc::EAFNOSUPPORT), "{f}");
        }
        // Netlink: route and uevent only; the protocol's high bits are ignored like the
        // kernel's (an int).
        let netlink = |p: u64| {
            call(
                libc::SYS_socket,
                [libc::AF_NETLINK as u64, libc::SOCK_RAW as u64, p, 0, 0, 0],
            )
        };
        for p in [libc::NETLINK_ROUTE, libc::NETLINK_KOBJECT_UEVENT] {
            assert_eq!(netlink(p as u64), Verdict::Allow, "{p}");
            assert_eq!(netlink(0xffff_0000_0000_0000 | p as u64), Verdict::Allow);
        }
        for p in [
            libc::NETLINK_NETFILTER,
            libc::NETLINK_XFRM,
            libc::NETLINK_GENERIC,
            libc::NETLINK_SOCK_DIAG,
            libc::NETLINK_AUDIT,
            libc::NETLINK_CRYPTO,
            libc::NETLINK_RDMA,
            -1,
        ] {
            assert_eq!(
                netlink(p as u32 as u64),
                Verdict::Errno(libc::EPROTONOSUPPORT),
                "{p}"
            );
        }
        // The protocol matters only for netlink.
        assert_eq!(
            call(
                libc::SYS_socket,
                [libc::AF_INET as u64, libc::SOCK_RAW as u64, 12, 0, 0, 0]
            ),
            Verdict::Allow
        );
        // Other calls are not affected by the socket rule.
        assert_eq!(
            call(libc::SYS_getpid, [libc::AF_NETLINK as u64, 0, 12, 0, 0, 0]),
            Verdict::Allow
        );
    }

    #[test]
    fn only_the_native_abi() {
        let prog = instructions();
        // i386 (`int 0x80`): getpid is 20 there, keyctl 288.
        const AUDIT_ARCH_I386: u32 = 0x4000_0003;
        for nr in [20, 288] {
            assert_eq!(
                run(&prog, AUDIT_ARCH_I386, nr, [0; 6]),
                Verdict::KillProcess
            );
        }
        // x32: the x86-64 number with the x32 bit.
        assert_eq!(
            run(
                &prog,
                NATIVE_ARCH,
                X32_SYSCALL_BIT | libc::SYS_keyctl as u32,
                [0; 6]
            ),
            Verdict::Errno(libc::ENOSYS)
        );
        assert_eq!(
            run(&prog, NATIVE_ARCH, X32_SYSCALL_BIT | 39, [0; 6]),
            Verdict::Errno(libc::ENOSYS)
        );
    }

    #[test]
    fn program_bytes_and_pipe() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let bytes = program().unwrap();
        assert_eq!(bytes.len(), instructions().len() * 8);
        // BPF_MAXINSNS.
        assert!(instructions().len() <= 4096);
        let fd = pipe().unwrap();
        let flags = rustix::io::fcntl_getfd(&fd).unwrap();
        assert!(!flags.contains(rustix::io::FdFlags::CLOEXEC), "inherited");
        assert!(fd.as_raw_fd() > 2);
        let mut read = Vec::new();
        std::fs::File::from(fd).read_to_end(&mut read).unwrap();
        assert_eq!(read, bytes);
    }
}

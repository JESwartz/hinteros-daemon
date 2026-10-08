//! The few kernel interfaces of the framebuffer splash ([`crate::splash`]) that need `ioctl`:
//! the framebuffer's geometry and pixel format (`FBIOGET_VSCREENINFO`, `FBIOGET_FSCREENINFO`),
//! a virtual console's display mode (`KDGETMODE`/`KDSETMODE`), the active console
//! (`VT_GETSTATE`) and whether a session controller (logind on behalf of a compositor) holds it
//! (`VT_GETMODE`), plus marking inherited descriptors close-on-exec before the painter is spawned.
//!
//! The only module of this crate with `unsafe`: each call passes a `#[repr(C)]` mirror of the
//! kernel's struct (include/uapi/linux/fb.h, kd.h, vt.h) to the matching opcode, or an integer
//! for `KDSETMODE`. Nothing else here is unsafe; callers get plain `io::Result`s.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsFd, BorrowedFd, RawFd};

use rustix::ioctl::{Getter, IntegerSetter, Opcode, ioctl};

const FBIOGET_VSCREENINFO: Opcode = 0x4600;
const FBIOGET_FSCREENINFO: Opcode = 0x4602;
const KDSETMODE: Opcode = 0x4B3A;
const KDGETMODE: Opcode = 0x4B3B;
const VT_GETMODE: Opcode = 0x5601;
const VT_GETSTATE: Opcode = 0x5603;

pub const KD_TEXT: i32 = 0;
pub const KD_GRAPHICS: i32 = 1;
const VT_PROCESS: i8 = 1;

/// `struct fb_bitfield`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_var_screeninfo` (160 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FbVar {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FbFix {
    pub id: [u8; 16],
    pub smem_start: std::ffi::c_ulong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: std::ffi::c_ulong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

/// `struct vt_stat`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtStat {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

/// `struct vt_mode`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtMode {
    mode: i8,
    waitv: i8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

/// The framebuffer's variable and fixed screen info.
pub fn fb_info(fb: impl AsFd) -> io::Result<(FbVar, FbFix)> {
    // SAFETY: FBIOGET_VSCREENINFO / FBIOGET_FSCREENINFO write exactly a `struct
    // fb_var_screeninfo` / `struct fb_fix_screeninfo`, which `FbVar` / `FbFix` mirror.
    let var = unsafe { ioctl(fb.as_fd(), Getter::<FBIOGET_VSCREENINFO, FbVar>::new())? };
    let fix = unsafe { ioctl(fb.as_fd(), Getter::<FBIOGET_FSCREENINFO, FbFix>::new())? };
    Ok((var, fix))
}

/// The console's display mode ([`KD_TEXT`] or [`KD_GRAPHICS`]).
pub fn kd_mode(tty: impl AsFd) -> io::Result<i32> {
    // SAFETY: KDGETMODE writes an `int`.
    Ok(unsafe { ioctl(tty.as_fd(), Getter::<KDGETMODE, std::ffi::c_int>::new())? })
}

/// Set the console's display mode ([`KD_TEXT`] or [`KD_GRAPHICS`]).
pub fn set_kd_mode(tty: impl AsFd, mode: i32) -> io::Result<()> {
    let mode = if mode == KD_GRAPHICS {
        KD_GRAPHICS
    } else {
        KD_TEXT
    };
    // SAFETY: KDSETMODE takes the mode as an integer argument; only KD_TEXT or KD_GRAPHICS.
    unsafe {
        ioctl(
            tty.as_fd(),
            IntegerSetter::<KDSETMODE>::new_usize(mode as usize),
        )?
    };
    Ok(())
}

/// The number of the foreground virtual console.
pub fn active_vt(tty: impl AsFd) -> io::Result<u16> {
    // SAFETY: VT_GETSTATE writes a `struct vt_stat`, which `VtStat` mirrors.
    let st = unsafe { ioctl(tty.as_fd(), Getter::<VT_GETSTATE, VtStat>::new())? };
    Ok(st.v_active)
}

/// Whether a process controls the console's switching (`VT_PROCESS`): logind does for a session
/// whose compositor took control of it, and gives it back (`VT_AUTO`) when that session ends.
pub fn vt_controlled(tty: impl AsFd) -> io::Result<bool> {
    // SAFETY: VT_GETMODE writes a `struct vt_mode`, which `VtMode` mirrors.
    let m = unsafe { ioctl(tty.as_fd(), Getter::<VT_GETMODE, VtMode>::new())? };
    Ok(m.mode == VT_PROCESS)
}

/// Mark every descriptor from 3 up close-on-exec (what pam_exec or greetd may have left open),
/// so a spawned process starts with only stdin, stdout and stderr.
pub fn cloexec_inherited() {
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    let fds: Vec<RawFd> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd >= 3)
        .collect();
    for fd in fds {
        // SAFETY: only for the duration of two fcntl calls on a descriptor number that was open
        // in this process a moment ago (the directory's own descriptor is closed by now: EBADF,
        // ignored); setting FD_CLOEXEC changes nothing about who owns or uses it.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        if let Ok(flags) = rustix::io::fcntl_getfd(fd) {
            let _ = rustix::io::fcntl_setfd(fd, flags | rustix::io::FdFlags::CLOEXEC);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(std::mem::size_of::<FbVar>(), 160);
        assert_eq!(std::mem::size_of::<FbFix>(), 80);
        assert_eq!(std::mem::size_of::<VtStat>(), 6);
        assert_eq!(std::mem::size_of::<VtMode>(), 8);
        assert_eq!(std::mem::offset_of!(FbFix, line_length), 48);
        assert_eq!(std::mem::offset_of!(FbVar, red), 32);
        assert_eq!(std::mem::offset_of!(FbVar, nonstd), 80);
    }

    #[test]
    fn ioctls_on_something_else_fail_cleanly() {
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(fb_info(&f).is_err());
        assert!(kd_mode(&f).is_err());
        assert!(active_vt(&f).is_err());
        assert!(vt_controlled(&f).is_err());
    }
}

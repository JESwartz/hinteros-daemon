//! Storage: what uses the disk, as the ring's tree.
//!
//! The `storage_measure` job ([`crate::runner`]) gathers the facts: the store's filesystem
//! (statvfs), the NAR size of every store path and which are live, the closure of every system
//! and app-env generation, and each home with [`measure_home`] (a walk by directory descriptor
//! that never follows a symlink, not even one swapped in while it runs, never leaves the home's
//! filesystem and never reads a file; only Steam's `libraryfolders.vdf` and `appmanifest_*.acf`
//! are read, at most 1 MiB each, to name the games), and `/srv`. This
//! module turns them into [`StorageNode`] trees whose children always add up to their parent, and writes the
//! result to `/var/lib/hinteros/storage.json` (root, 0600): the public view (top level, every
//! profile's total, the system/apps breakdown, the clean-up estimates) and the detailed trees
//! of the homes, which only the service hands out (the profile itself, or with the admin PIN).
//!
//! Nothing here logs a file or folder name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use hinteros_api::system::{
    CleanupEstimate, CleanupEstimates, DiskUsage, MAX_KEEP, Measurement, NodeKind, StorageNode,
};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat, fstat, openat, statat};
use serde::{Deserialize, Serialize};

use crate::fsutil;

/// Format of `storage.json`, a cache: any other version is discarded.
pub const STORED_VERSION: u32 = 1;

/// What the measure job writes and the service reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub version: u32,
    /// The public view (`storage.get`).
    pub measurement: Measurement,
    /// Detailed home trees by profile id or `guest` (`storage.tree`; never public).
    pub homes: BTreeMap<String, StorageNode>,
}

impl Stored {
    pub fn load(path: &Path) -> Option<Self> {
        let s: Stored = crate::job::read_json(path)?;
        (s.version == STORED_VERSION).then_some(s)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fsutil::create_dir_all(parent, 0o777)?;
        }
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        fsutil::write_atomic(path, &bytes, 0o600)
    }

    /// Remove the detailed tree of home `id` (the guest's, when its home is wiped: the next
    /// guest must not see the folder names). Returns whether there was one.
    pub fn drop_home(path: &Path, id: &str) -> std::io::Result<bool> {
        let Some(mut s) = Self::load(path) else {
            return Ok(false);
        };
        if s.homes.remove(id).is_none() {
            return Ok(false);
        }
        s.save(path)?;
        Ok(true)
    }
}

/// statvfs of `dir`: total, free (available blocks) and used (total minus free, so the three
/// add up, as in the ring). On btrfs it is the whole filesystem from any subvolume; available
/// is below the free blocks by what the filesystem holds back for metadata, which counts as used
/// here and in [`top_level`].
pub fn disk_usage(dir: &Path) -> Option<DiskUsage> {
    let s = rustix::fs::statvfs(dir).ok()?;
    let frsize = s.f_frsize.max(1);
    Some(usage(
        s.f_blocks.saturating_mul(frsize),
        s.f_bavail.saturating_mul(frsize),
    ))
}

fn usage(total: u64, available: u64) -> DiskUsage {
    let free = available.min(total);
    DiskUsage {
        total_bytes: total,
        used_bytes: total - free,
        free_bytes: free,
    }
}

/// Write out what the kernel holds for the filesystem of `dir`. Btrfs gives freed space back when
/// a transaction commits, so statvfs right after a garbage collection shows too little.
pub fn sync_filesystem(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = rustix::fs::syncfs(&f);
    }
}

/// The filesystem `path` is on, from `/proc/self/mountinfo`. `st_dev` and `f_fsid` do not say:
/// every btrfs subvolume has its own, on one filesystem and one free space.
pub fn filesystem_of(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    filesystem_in(&mountinfo, &path)
}

/// [`filesystem_of`] an absolute, canonical `path`, given the text of `mountinfo`: the mount
/// source of the longest mount point above it (of equal ones the last, which hides the others).
/// A source without a device (tmpfs, overlay) says nothing: then `major:minor`.
pub fn filesystem_in(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // id parent major:minor root mountpoint options [tags...] - type source superoptions
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        let (Some(dev), Some(point), Some(source)) =
            (fields.get(2), fields.get(4), fields.get(dash + 2))
        else {
            continue;
        };
        let point = unescape_mount(point);
        if !path.starts_with(&point) || best.as_ref().is_some_and(|(len, _)| point.len() < *len) {
            continue;
        }
        let id = if source.starts_with('/') {
            (*source).to_owned()
        } else {
            format!("dev:{dev}")
        };
        best = Some((point.len(), id));
    }
    best.map(|(_, id)| id)
}

/// `mountinfo` writes space, tab, newline and backslash in octal.
fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

// ---------------------------------------------------------------------------------------------
// Homes

/// Limits that keep a home's tree small (depth-limited, size-thresholded).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Folder levels below the home that are shown.
    pub max_depth: usize,
    /// A folder must be at least this big…
    pub min_bytes: u64,
    /// …and at least this many thousandths of the home.
    pub min_permille: u64,
    /// Folders shown per folder (the rest go into `files`).
    pub max_children: usize,
    /// Nodes in a whole tree (the threshold rises until it fits).
    pub max_nodes: usize,
    /// Characters of a name.
    pub max_name: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 6,
            min_bytes: 1 << 20,
            min_permille: 5,
            max_children: 16,
            max_nodes: 400,
            max_name: 128,
        }
    }
}

/// A folder as walked: its own block and loose files in `files`, its sub-folders (those of at
/// least the floor size, within the depth) in `dirs`; `bytes` is everything below it.
#[derive(Debug, Clone, Default)]
struct RawDir {
    name: String,
    bytes: u64,
    files: u64,
    dirs: Vec<RawDir>,
}

/// Folders are opened with these, relative to their parent's descriptor: the walk runs as root
/// in homes their (untrusted) users can change under it, so it never resolves a path. A folder
/// swapped for a symlink after it was looked at fails to open and is left out.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Open the folder at `path` (its last component not followed) and stat it.
fn open_dir(path: &Path) -> Option<(OwnedFd, Stat)> {
    let fd = rustix::fs::open(path, DIR_FLAGS, Mode::empty()).ok()?;
    let st = fstat(&fd).ok()?;
    Some((fd, st))
}

/// Open folder `name` in the open folder `parent` without following a symlink, and stat it.
fn open_dir_at(parent: &OwnedFd, name: &str) -> Option<(OwnedFd, Stat)> {
    let c = CString::new(name).ok()?;
    let fd = openat(parent, c.as_c_str(), DIR_FLAGS, Mode::empty()).ok()?;
    let st = fstat(&fd).ok()?;
    Some((fd, st))
}

/// Allocated bytes of a stat.
fn block_bytes(st: &Stat) -> u64 {
    u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512)
}

fn is_dir(st: &Stat) -> bool {
    FileType::from_raw_mode(st.st_mode) == FileType::Directory
}

struct Walk<'a> {
    dev: u64,
    /// Folders not counted in the tree (the Steam games, counted on their own).
    exclude: HashSet<(u64, u64)>,
    /// Files with more than one link already counted.
    seen: HashSet<(u64, u64)>,
    limits: Limits,
    entries: u64,
    cancel: &'a dyn Fn() -> bool,
    cancelled: bool,
}

impl Walk<'_> {
    fn tick(&mut self) -> bool {
        self.entries += 1;
        if self.entries.is_multiple_of(4096) && (self.cancel)() {
            self.cancelled = true;
        }
        self.cancelled
    }

    fn file_bytes(&mut self, st: &Stat) -> u64 {
        if st.st_nlink > 1 && !is_dir(st) && !self.seen.insert((st.st_dev, st.st_ino)) {
            return 0;
        }
        block_bytes(st)
    }

    /// Walk the open folder `fd` (already checked to be on our device; `st` its stat). The walk
    /// reads entries through `fd` itself (one descriptor per level, closed when the level is done:
    /// a deep tree must not run out of descriptors).
    /// `keep_tree`: record sub-folders (false below the depth limit and for the games).
    fn dir(&mut self, fd: OwnedFd, st: &Stat, depth: usize, keep_tree: bool) -> RawDir {
        let mut out = RawDir {
            name: String::new(),
            bytes: 0,
            files: block_bytes(st),
            dirs: Vec::new(),
        };
        let Ok(mut entries) = Dir::new(fd) else {
            out.bytes = out.files;
            return out;
        };
        while let Some(entry) = entries.read() {
            let Ok(entry) = entry else { break };
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            if self.tick() {
                break;
            }
            // The stream's own descriptor: statat and openat do not move its position.
            let Ok(fd) = entries.fd() else { break };
            let Ok(m) = statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            if m.st_dev != self.dev {
                // A mount point or a nested subvolume: not counted (nor its directory).
                continue;
            }
            if !is_dir(&m) {
                out.files += self.file_bytes(&m);
                continue;
            }
            // What is walked is the folder opened now, checked again (it may have been swapped
            // since the stat above).
            let Ok(sub_fd) = openat(fd, name, DIR_FLAGS, Mode::empty()) else {
                continue;
            };
            let Ok(sm) = fstat(&sub_fd) else { continue };
            if sm.st_dev != self.dev || self.exclude.contains(&(sm.st_dev, sm.st_ino)) {
                continue;
            }
            let mut sub = self.dir(
                sub_fd,
                &sm,
                depth + 1,
                keep_tree && depth < self.limits.max_depth,
            );
            if keep_tree && depth < self.limits.max_depth && sub.bytes >= self.limits.min_bytes {
                sub.name = clean_name(&name.to_string_lossy(), self.limits.max_name);
                out.dirs.push(sub);
            } else {
                out.files += sub.bytes;
            }
        }
        out.bytes = out.files + out.dirs.iter().map(|d| d.bytes).sum::<u64>();
        out
    }
}

/// A name as shown: at most `max` characters, control characters replaced.
pub fn clean_name(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// One installed Steam game in a home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGame {
    pub app_id: u32,
    pub name: String,
    pub dir: PathBuf,
}

/// The quoted strings of a VDF/ACF text, in order (escapes `\\` and `\"` undone).
fn vdf_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some(other) => s.push(other),
                    None => break,
                },
                '"' => break,
                c => s.push(c),
            }
        }
        out.push(s);
    }
    out
}

/// The value after the first `key` (case-insensitive), as VDF writes `"key" "value"`.
fn vdf_value<'a>(tokens: &'a [String], key: &str) -> Option<&'a str> {
    tokens
        .windows(2)
        .find(|w| w[0].eq_ignore_ascii_case(key))
        .map(|w| w[1].as_str())
}

/// Largest Steam file read.
const MAX_SMALL: u64 = 1 << 20;

/// Read the small regular file `name` in the open folder `dir` without following a symlink
/// (Steam's own files only). Opened non-blocking (a FIFO does not hang the walk), checked on the
/// open file, and never more than [`MAX_SMALL`] bytes read.
fn read_small_at(dir: &OwnedFd, name: &str) -> Option<String> {
    use std::io::Read;
    let c = CString::new(name).ok()?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = openat(dir, c.as_c_str(), flags, Mode::empty()).ok()?;
    let st = fstat(&fd).ok()?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || u64::try_from(st.st_size).map_or(true, |n| n > MAX_SMALL)
    {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_SMALL)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Open `path`, which must be `home` or below it, component by component from the open home
/// without following a symlink (lexically clean and absolute, else `None`).
fn open_in_home(home_fd: &OwnedFd, home: &Path, path: &Path) -> Option<(OwnedFd, Stat)> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let rest = path.strip_prefix(home).ok()?;
    let mut fd = rustix::io::dup(home_fd).ok()?;
    let mut st = fstat(&fd).ok()?;
    for c in rest.components() {
        let Component::Normal(part) = c else {
            continue;
        };
        (fd, st) = open_dir_at(&fd, part.to_str()?)?;
    }
    Some((fd, st))
}

/// The Steam library folders inside `home` (open): Steam's own folder and every `"path"` of its
/// `libraryfolders.vdf` that is a real folder in the home.
fn steam_libraries(home_fd: &OwnedFd, home: &Path) -> Vec<(PathBuf, OwnedFd)> {
    let main = home.join(".local/share/Steam");
    let Some((main_fd, main_st)) = open_in_home(home_fd, home, &main) else {
        return Vec::new();
    };
    let mut seen = vec![(main_st.st_dev, main_st.st_ino)];
    let mut texts = Vec::new();
    for sub in ["steamapps", "config"] {
        if let Some((fd, _)) = open_dir_at(&main_fd, sub)
            && let Some(text) = read_small_at(&fd, "libraryfolders.vdf")
        {
            texts.push(text);
        }
    }
    let mut libs = vec![(main, main_fd)];
    for text in texts {
        let tokens = vdf_tokens(&text);
        for w in tokens.windows(2) {
            if !w[0].eq_ignore_ascii_case("path") {
                continue;
            }
            let p = PathBuf::from(&w[1]);
            if let Some((fd, st)) = open_in_home(home_fd, home, &p)
                && !seen.contains(&(st.st_dev, st.st_ino))
            {
                seen.push((st.st_dev, st.st_ino));
                libs.push((p, fd));
            }
        }
    }
    libs
}

/// The installed games of the libraries in `home` (open), named from their
/// `appmanifest_*.acf`, each with its folder open.
fn steam_games(home_fd: &OwnedFd, home: &Path) -> Vec<(SteamGame, OwnedFd, Stat)> {
    let mut games: Vec<(SteamGame, OwnedFd, Stat)> = Vec::new();
    for (lib, lib_fd) in steam_libraries(home_fd, home) {
        let Some((apps_fd, _)) = open_dir_at(&lib_fd, "steamapps") else {
            continue;
        };
        let Ok(mut entries) = rustix::io::dup(&apps_fd).and_then(Dir::new) else {
            continue;
        };
        let common = open_dir_at(&apps_fd, "common");
        while let Some(Ok(e)) = entries.read() {
            let Ok(file) = e.file_name().to_str() else {
                continue;
            };
            let Some(id) = file
                .strip_prefix("appmanifest_")
                .and_then(|r| r.strip_suffix(".acf"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(text) = read_small_at(&apps_fd, file) else {
                continue;
            };
            let tokens = vdf_tokens(&text);
            let Some(installdir) = vdf_value(&tokens, "installdir") else {
                continue;
            };
            // One plain path component.
            let mut comps = Path::new(installdir).components();
            if !matches!(
                (comps.next(), comps.next()),
                (Some(Component::Normal(_)), None)
            ) {
                continue;
            }
            let Some((dir_fd, st)) = common
                .as_ref()
                .and_then(|(c, _)| open_dir_at(c, installdir))
            else {
                continue;
            };
            if games
                .iter()
                .any(|(g, _, s)| g.app_id == id || (s.st_dev, s.st_ino) == (st.st_dev, st.st_ino))
            {
                continue;
            }
            let name = vdf_value(&tokens, "name")
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(installdir);
            games.push((
                SteamGame {
                    app_id: id,
                    name: clean_name(name.trim(), 128),
                    dir: lib.join("steamapps/common").join(installdir),
                },
                dir_fd,
                st,
            ));
        }
    }
    games
}

/// Measure `home` into a detailed tree of `kind` (profile or guest). `None`: not a folder, or
/// cancelled (`cancel` is asked every few thousand entries). Blocks are the uncompressed size on
/// btrfs: see [`top_level`].
pub fn measure_home(
    home: &Path,
    kind: NodeKind,
    limits: Limits,
    cancel: &dyn Fn() -> bool,
) -> Option<StorageNode> {
    let (home_fd, st) = open_dir(home)?;
    let dev = st.st_dev;
    let games = steam_games(&home_fd, home);
    let mut walk = Walk {
        dev,
        exclude: HashSet::new(),
        seen: HashSet::new(),
        limits,
        entries: 0,
        cancel,
        cancelled: false,
    };
    let mut game_nodes = Vec::new();
    for (g, fd, m) in games {
        if m.st_dev != dev || !walk.exclude.insert((m.st_dev, m.st_ino)) {
            continue;
        }
        let raw = walk.dir(fd, &m, limits.max_depth, false);
        let mut n = StorageNode::new(NodeKind::SteamGame, raw.bytes);
        n.name = Some(g.name.clone());
        n.app_id = Some(g.app_id);
        game_nodes.push(n);
    }
    let raw = walk.dir(home_fd, &st, 0, true);
    if walk.cancelled {
        return None;
    }
    Some(home_tree(kind, raw, game_nodes, limits))
}

/// The total of `/srv`, walked like a home, for the `media` node: no tree, nobody sees inside it
/// here. `None`: not a folder, or cancelled.
pub fn measure_media(dir: &Path, limits: Limits, cancel: &dyn Fn() -> bool) -> Option<StorageNode> {
    let (fd, st) = open_dir(dir)?;
    let mut walk = Walk {
        dev: st.st_dev,
        exclude: HashSet::new(),
        seen: HashSet::new(),
        limits,
        entries: 0,
        cancel,
        cancelled: false,
    };
    let raw = walk.dir(fd, &st, limits.max_depth, false);
    (!walk.cancelled).then(|| StorageNode::new(NodeKind::Media, raw.bytes))
}

/// Build the shown tree: the threshold is max(floor, permille of the total), raised until the
/// tree has at most `max_nodes` nodes.
fn home_tree(
    kind: NodeKind,
    raw: RawDir,
    mut games: Vec<StorageNode>,
    limits: Limits,
) -> StorageNode {
    let games_bytes: u64 = games.iter().map(|g| g.bytes).sum();
    let total = raw.bytes + games_bytes;
    games.retain(|g| g.bytes > 0);
    sort_named(&mut games);
    let mut threshold = limits
        .min_bytes
        .max(total.saturating_mul(limits.min_permille) / 1000);
    loop {
        let mut root = StorageNode::new(kind, total);
        if games_bytes > 0 {
            let mut steam = StorageNode::new(NodeKind::Steam, games_bytes);
            steam.children = games.clone();
            root.children.push(steam);
        }
        let (dirs, files) = shown(&raw, threshold, limits.max_children);
        root.children.extend(dirs);
        sort_named(&mut root.children);
        if files > 0 {
            root.children.push(StorageNode::new(NodeKind::Files, files));
        }
        if count(&root) <= limits.max_nodes || threshold >= total {
            return root;
        }
        threshold = threshold.saturating_mul(2);
    }
}

/// The sub-folders of `raw` shown at `threshold` (at most `max` of them), and the bytes left
/// for its `files` node.
fn shown(raw: &RawDir, threshold: u64, max: usize) -> (Vec<StorageNode>, u64) {
    let mut dirs: Vec<&RawDir> = raw.dirs.iter().filter(|d| d.bytes >= threshold).collect();
    dirs.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    dirs.truncate(max);
    let shown_bytes: u64 = dirs.iter().map(|d| d.bytes).sum();
    let nodes = dirs
        .into_iter()
        .map(|d| {
            let mut n = StorageNode::new(NodeKind::Dir, d.bytes);
            n.name = Some(d.name.clone());
            let (children, files) = shown(d, threshold, max);
            n.children = children;
            if !n.children.is_empty() && files > 0 {
                n.children.push(StorageNode::new(NodeKind::Files, files));
            }
            n
        })
        .collect();
    (nodes, raw.bytes - shown_bytes)
}

fn count(n: &StorageNode) -> usize {
    1 + n.children.iter().map(count).sum::<usize>()
}

/// Largest first, then by name (stable for equal sizes).
fn sort_named(nodes: &mut [StorageNode]) {
    nodes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
}

// ---------------------------------------------------------------------------------------------
// The store

/// One generation and its closure (store paths).
#[derive(Debug, Clone, Default)]
pub struct GenFacts {
    /// `None`: a single link without generations (the guest's env).
    pub generation: Option<u32>,
    pub date: i64,
    pub current: bool,
    pub booted: bool,
    pub closure: Vec<String>,
}

/// An app-env owner (a profile id, `starter`, `guest`) and its generations, newest first.
#[derive(Debug, Clone, Default)]
pub struct OwnerFacts {
    pub id: String,
    pub gens: Vec<GenFacts>,
}

/// The store: every valid path's NAR size, and which paths are live.
#[derive(Debug, Clone, Default)]
pub struct StoreFacts {
    pub sizes: HashMap<String, u64>,
    pub live: HashSet<String>,
}

/// Store paths interned to indices.
struct Interner<'a> {
    index: HashMap<&'a str, usize>,
    sizes: Vec<u64>,
}

impl<'a> Interner<'a> {
    fn new(facts: &'a StoreFacts) -> Self {
        let mut index = HashMap::with_capacity(facts.sizes.len());
        let mut sizes = Vec::with_capacity(facts.sizes.len());
        for (p, s) in &facts.sizes {
            index.insert(p.as_str(), sizes.len());
            sizes.push(*s);
        }
        Self { index, sizes }
    }

    /// A closure as sorted, unique indices (paths without a known size are left out).
    fn set(&self, closure: &[String]) -> Vec<usize> {
        let mut v: Vec<usize> = closure
            .iter()
            .filter_map(|p| self.index.get(p.as_str()).copied())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn bytes(&self, set: impl IntoIterator<Item = usize>) -> u64 {
        set.into_iter().map(|i| self.sizes[i]).sum()
    }
}

/// The store side of a measurement.
#[derive(Debug, Clone)]
pub struct StoreTree {
    pub system: StorageNode,
    pub apps: StorageNode,
    pub garbage_bytes: u64,
    pub estimates: CleanupEstimates,
}

/// Generations with their closures as index sets.
type GenSets<'a> = Vec<(&'a GenFacts, Vec<usize>)>;

/// `generation` + `common` children of a node holding `gens` (each a set of indices),
/// restricted to `within` (when given).
fn generation_children(
    int: &Interner,
    gens: &[(&GenFacts, Vec<usize>)],
    within: Option<&[bool]>,
) -> (u64, Vec<StorageNode>) {
    let mut count: HashMap<usize, u32> = HashMap::new();
    for (_, set) in gens {
        for &i in set {
            if within.is_none_or(|w| w[i]) {
                *count.entry(i).or_insert(0) += 1;
            }
        }
    }
    let total = int.bytes(count.keys().copied());
    let mut children = Vec::new();
    let mut unique_sum = 0;
    for (g, set) in gens {
        let Some(generation) = g.generation else {
            continue;
        };
        let unique = int.bytes(set.iter().copied().filter(|i| count.get(i) == Some(&1)));
        unique_sum += unique;
        if unique > 0 {
            let mut n = StorageNode::new(NodeKind::Generation, unique);
            n.generation = Some(generation);
            n.date = Some(g.date);
            n.current = Some(g.current);
            n.booted = Some(g.booted);
            children.push(n);
        }
    }
    children.sort_by_key(|c| std::cmp::Reverse(c.generation));
    if !children.is_empty() && total > unique_sum {
        children.push(StorageNode::new(NodeKind::Common, total - unique_sum));
    }
    (total, children)
}

/// Which generations a clean-up with `keep` removes: all but the newest `keep` (by number), the
/// current and the booted one.
pub fn removed_by(gens: &[GenFacts], keep: u32) -> Vec<u32> {
    let mut numbers: Vec<(u32, bool)> = gens
        .iter()
        .filter_map(|g| g.generation.map(|n| (n, g.current || g.booted)))
        .collect();
    numbers.sort_by_key(|n| std::cmp::Reverse(n.0));
    numbers
        .into_iter()
        .enumerate()
        .filter(|(i, (_, pinned))| *i >= keep as usize && !pinned)
        .map(|(_, (n, _))| n)
        .collect()
}

/// Turn the store facts into the `system` and `apps` nodes, the garbage and the estimates.
pub fn store_tree(facts: &StoreFacts, system: &[GenFacts], owners: &[OwnerFacts]) -> StoreTree {
    let int = Interner::new(facts);
    let n = int.sizes.len();
    let garbage_bytes = facts
        .sizes
        .iter()
        .filter(|(p, _)| !facts.live.contains(*p))
        .map(|(_, s)| s)
        .sum();

    // System: every system generation's closure.
    let sys: Vec<(&GenFacts, Vec<usize>)> =
        system.iter().map(|g| (g, int.set(&g.closure))).collect();
    let mut in_system = vec![false; n];
    for (_, set) in &sys {
        for &i in set {
            in_system[i] = true;
        }
    }
    let (sys_bytes, sys_children) = generation_children(&int, &sys, None);
    let mut system_node = StorageNode::new(NodeKind::System, sys_bytes);
    system_node.children = sys_children;

    // Apps: what the envs hold outside the system; in two or more owners' envs: shared.
    let owner_sets: Vec<(&OwnerFacts, GenSets)> = owners
        .iter()
        .map(|o| (o, o.gens.iter().map(|g| (g, int.set(&g.closure))).collect()))
        .collect();
    let mut owners_of = vec![0u32; n];
    for (_, gens) in &owner_sets {
        let mut mine: HashSet<usize> = HashSet::new();
        for (_, set) in gens {
            mine.extend(set.iter().copied().filter(|i| !in_system[*i]));
        }
        for i in mine {
            owners_of[i] += 1;
        }
    }
    let shared = int.bytes((0..n).filter(|i| owners_of[*i] >= 2));
    let exclusive: Vec<bool> = (0..n).map(|i| owners_of[i] == 1).collect();
    let mut apps_node = StorageNode::new(NodeKind::Apps, 0);
    let mut owner_nodes = Vec::new();
    for (o, gens) in &owner_sets {
        let (bytes, children) = generation_children(&int, gens, Some(&exclusive));
        let mut node = StorageNode::new(NodeKind::AppEnv, bytes);
        node.id = Some(o.id.clone());
        node.children = children;
        owner_nodes.push(node);
    }
    sort_named(&mut owner_nodes);
    apps_node.bytes = owner_nodes.iter().map(|o| o.bytes).sum::<u64>() + shared;
    apps_node.children = owner_nodes;
    if shared > 0 {
        apps_node
            .children
            .push(StorageNode::new(NodeKind::Shared, shared));
    }

    // Estimates: what each clean-up would free with keep 1..10.
    let all_apps: Vec<bool> = {
        let mut v = vec![false; n];
        for (_, gens) in &owner_sets {
            for (_, set) in gens {
                for &i in set {
                    v[i] = true;
                }
            }
        }
        v
    };
    let freed = |removed: &[&Vec<usize>], protected: &[bool]| -> u64 {
        let mut gone: HashSet<usize> = HashSet::new();
        for set in removed {
            gone.extend(set.iter().copied().filter(|i| !protected[*i]));
        }
        garbage_bytes + int.bytes(gone)
    };
    let mut estimates = CleanupEstimates::default();
    for keep in 1..=MAX_KEEP {
        // System: kept generations and every app env protect their paths.
        let removed = removed_by(system, keep);
        let mut protected = all_apps.clone();
        let mut gone_sets = Vec::new();
        for (g, set) in &sys {
            if g.generation.is_some_and(|x| removed.contains(&x)) {
                gone_sets.push(set);
            } else {
                for &i in set {
                    protected[i] = true;
                }
            }
        }
        estimates.system.push(CleanupEstimate {
            keep,
            generations: removed.len() as u32,
            bytes: freed(&gone_sets, &protected),
        });

        // Profiles: the system, kept app generations and single links protect theirs.
        let mut protected = in_system.clone();
        let mut gone_sets = Vec::new();
        let mut count = 0u32;
        for (o, gens) in &owner_sets {
            let removed = removed_by(&o.gens, keep);
            count += removed.len() as u32;
            for (g, set) in gens {
                if g.generation.is_some_and(|x| removed.contains(&x)) {
                    gone_sets.push(set);
                } else {
                    for &i in set {
                        protected[i] = true;
                    }
                }
            }
        }
        estimates.profiles.push(CleanupEstimate {
            keep,
            generations: count,
            bytes: freed(&gone_sets, &protected),
        });
    }
    StoreTree {
        system: system_node,
        apps: apps_node,
        garbage_bytes,
        estimates,
    }
}

/// A profile's (or the guest's) node for the top level: its total, no children.
pub fn summary(tree: &StorageNode) -> StorageNode {
    let mut n = StorageNode::new(tree.kind, tree.bytes);
    n.id = tree.id.clone();
    n
}

/// `parts` scaled down to add up to exactly `target` (at most their sum): each gets its rounded
/// down share, and what rounding left goes one byte each to the largest fractions.
fn shares(parts: &[u64], target: u64) -> Vec<u64> {
    let total: u128 = parts.iter().map(|&p| u128::from(p)).sum();
    if total == 0 {
        return vec![0; parts.len()];
    }
    let exact = parts.iter().map(|&p| u128::from(p) * u128::from(target));
    let mut out: Vec<u64> = exact.clone().map(|e| (e / total) as u64).collect();
    let mut order: Vec<usize> = (0..parts.len()).collect();
    let fractions: Vec<u128> = exact.map(|e| e % total).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(fractions[i]));
    let left = target.saturating_sub(out.iter().sum());
    for &i in order.iter().take(left as usize) {
        out[i] += 1;
    }
    out
}

/// `node` scaled down to `bytes`: its children get [`shares`] of it, all the way down, so they
/// still add up. A child scaled to nothing goes.
fn scale(node: &mut StorageNode, bytes: u64) {
    node.bytes = bytes;
    let sizes: Vec<u64> = node.children.iter().map(|c| c.bytes).collect();
    for (c, b) in node.children.iter_mut().zip(shares(&sizes, bytes)) {
        scale(c, b);
    }
    node.children.retain(|c| c.bytes > 0);
}

/// The top-level tree: system, apps, the homes (in the given order), media, other, free. The bool
/// says whether a home or `/srv` is on the store's filesystem (only those reduce `other`).
///
/// NAR sizes and `st_blocks` are uncompressed, so on a compressed filesystem what is known can be
/// more than the disk uses (total minus free). The known segments on the disk are then scaled
/// down in proportion to fill exactly that, and `other` is 0: the ring still adds up to the disk.
/// `homes` are scaled in place, with their detailed trees, so `storage.tree` agrees. The store's
/// garbage and clean-up estimates (NAR sizes too) are scaled by the store's factor (System and
/// Apps together), so a clean-up does not promise the uncompressed size.
pub fn top_level(
    disk: DiskUsage,
    store: &mut StoreTree,
    homes: &mut [(StorageNode, bool)],
    media: Option<(StorageNode, bool)>,
) -> StorageNode {
    let (mut system, mut apps) = (store.system.clone(), store.apps.clone());
    let mut media = media.filter(|(m, _)| m.bytes > 0);
    let used = disk.total_bytes.saturating_sub(disk.free_bytes);
    {
        let mut known: Vec<&mut StorageNode> = vec![&mut system, &mut apps];
        known.extend(homes.iter_mut().filter(|(_, on)| *on).map(|(h, _)| h));
        known.extend(media.iter_mut().filter(|(_, on)| *on).map(|(m, _)| m));
        let sizes: Vec<u64> = known.iter().map(|n| n.bytes).collect();
        if sizes.iter().sum::<u64>() > used {
            for (n, b) in known.into_iter().zip(shares(&sizes, used)) {
                scale(n, b);
            }
        }
    }
    let mut known = system.bytes + apps.bytes;
    let raw = store.system.bytes + store.apps.bytes;
    if known < raw {
        let f = |b: u64| (u128::from(b) * u128::from(known) / u128::from(raw)) as u64;
        store.garbage_bytes = f(store.garbage_bytes);
        let estimates = &mut store.estimates;
        for e in estimates
            .system
            .iter_mut()
            .chain(estimates.profiles.iter_mut())
        {
            e.bytes = f(e.bytes);
        }
    }
    let mut root = StorageNode::new(NodeKind::Disk, 0);
    root.children.push(system);
    root.children.push(apps);
    for (h, on_disk) in homes.iter() {
        if *on_disk {
            known += h.bytes;
        }
        root.children.push(summary(h));
    }
    if let Some((m, on_disk)) = media {
        if on_disk {
            known += m.bytes;
        }
        root.children.push(m);
    }
    let other = used.saturating_sub(known);
    root.children
        .retain(|c| c.bytes > 0 || matches!(c.kind, NodeKind::Profile | NodeKind::Guest));
    if other > 0 {
        root.children.push(StorageNode::new(NodeKind::Other, other));
    }
    root.children
        .push(StorageNode::new(NodeKind::Free, disk.free_bytes));
    root.bytes = root.children.iter().map(|c| c.bytes).sum();
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn sum_ok(n: &StorageNode) {
        if !n.children.is_empty() {
            let s: u64 = n.children.iter().map(|c| c.bytes).sum();
            assert_eq!(s, n.bytes, "children of {:?} {:?}", n.kind, n.name);
        }
        for c in &n.children {
            sum_ok(c);
        }
    }

    /// The folder names in a tree.
    fn dir_names(n: &StorageNode) -> Vec<String> {
        let mut out = Vec::new();
        if n.kind == NodeKind::Dir {
            out.extend(n.name.clone());
        }
        for c in &n.children {
            out.extend(dir_names(c));
        }
        out
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    const MIB: usize = 1 << 20;

    fn small() -> Limits {
        Limits {
            min_bytes: 64 * 1024,
            ..Limits::default()
        }
    }

    #[test]
    fn a_home_with_folders_and_steam_games() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        write(&home.join("Videos/a.mkv"), 2 * MIB);
        write(&home.join("Videos/deep/1/2/3/4/5/6/7/x"), MIB);
        write(&home.join("Documents/note"), 100);
        write(&home.join(".cache/c"), MIB);
        write(&home.join("loose"), 300 * 1024);
        // Steam's own library with one game, and a second library inside the home.
        let steam = home.join(".local/share/Steam");
        write(&steam.join("steamapps/common/Portal Two/data"), 3 * MIB);
        std::fs::write(
            steam.join("steamapps/appmanifest_620.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\t\"installdir\"\t\t\"Portal Two\"\n}\n",
        )
        .unwrap();
        let lib2 = home.join("Games/SteamLibrary");
        write(&lib2.join("steamapps/common/Hades/game.bin"), 2 * MIB);
        std::fs::write(
            lib2.join("steamapps/appmanifest_1145360.acf"),
            "\"AppState\" { \"appid\" \"1145360\" \"name\" \"Hades\" \"installdir\" \"Hades\" }",
        )
        .unwrap();
        // A manifest whose installdir escapes, and one without a folder: ignored.
        std::fs::write(
            lib2.join("steamapps/appmanifest_1.acf"),
            "\"AppState\" { \"name\" \"Evil\" \"installdir\" \"../../..\" }",
        )
        .unwrap();
        std::fs::write(
            lib2.join("steamapps/appmanifest_2.acf"),
            "\"AppState\" { \"name\" \"Gone\" \"installdir\" \"Gone\" }",
        )
        .unwrap();
        let outside = d.path().join("outside");
        write(&outside.join("steamapps/common/X/big"), 5 * MIB);
        std::fs::write(
            outside.join("steamapps/appmanifest_3.acf"),
            "\"AppState\" { \"name\" \"Outside\" \"installdir\" \"X\" }",
        )
        .unwrap();
        std::fs::write(
            steam.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"2\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"3\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                steam.display(),
                lib2.display(),
                outside.display(),
                home.join("Games/Linked").display()
            ),
        )
        .unwrap();
        // A symlink out of the home is not followed (its target is 5 MiB).
        symlink(&outside, home.join("link")).unwrap();
        // Manifests that are a FIFO (would block) or a link to an endless file: skipped.
        rustix::fs::mknodat(
            rustix::fs::CWD,
            lib2.join("steamapps/appmanifest_4.acf"),
            FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        symlink("/dev/zero", lib2.join("steamapps/appmanifest_5.acf")).unwrap();
        // A library whose `steamapps` is a link out of the home: not followed.
        let lib3 = home.join("Games/Linked");
        std::fs::create_dir_all(&lib3).unwrap();
        symlink(outside.join("steamapps"), lib3.join("steamapps")).unwrap();

        let h = measure_home(&home, NodeKind::Profile, small(), &|| false).unwrap();
        let t = &h;
        sum_ok(t);
        assert_eq!(t.kind, NodeKind::Profile);
        assert!(
            t.bytes > 9 * MIB as u64 && t.bytes < 13 * MIB as u64,
            "{}",
            t.bytes
        );
        let steam_node = t
            .children
            .iter()
            .find(|c| c.kind == NodeKind::Steam)
            .unwrap();
        let games: Vec<(&str, u32)> = steam_node
            .children
            .iter()
            .map(|g| (g.name.as_deref().unwrap(), g.app_id.unwrap()))
            .collect();
        assert_eq!(games, [("Portal 2", 620), ("Hades", 1_145_360)]);
        assert!(steam_node.children[0].bytes >= 3 * MIB as u64);
        // The games are not counted again in the folders.
        let all = dir_names(t);
        assert!(!all.contains(&"Portal Two".to_string()), "{all:?}");
        assert!(!all.contains(&"Hades".to_string()), "{all:?}");
        assert!(all.contains(&"Videos".to_string()), "{all:?}");
        assert!(all.contains(&".cache".to_string()), "{all:?}");
        // Small folders go into files; the depth limit holds.
        assert!(!all.contains(&"Documents".to_string()), "{all:?}");
        assert!(!all.contains(&"7".to_string()), "{all:?}");
        assert!(!all.contains(&"Outside".to_string()) && !all.contains(&"link".to_string()));
        assert!(t.children.last().unwrap().kind == NodeKind::Files);

        // Cancelled: nothing.
        assert!(
            measure_home(&d.path().join("nope"), NodeKind::Guest, small(), &|| false).is_none()
        );
    }

    #[test]
    fn the_tree_stays_small() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        for i in 0..40 {
            for j in 0..12 {
                write(&home.join(format!("d{i}/e{j}/f")), 70 * 1024);
            }
        }
        let limits = Limits {
            min_bytes: 64 * 1024,
            max_nodes: 60,
            ..Limits::default()
        };
        let h = measure_home(&home, NodeKind::Profile, limits, &|| false).unwrap();
        sum_ok(&h);
        assert!(count(&h) <= 60, "{}", count(&h));
        // At most 16 folders per folder.
        assert!(h.children.len() <= 17);
        // Hard links are counted once.
        let one = d.path().join("one");
        write(&one.join("a"), MIB);
        std::fs::hard_link(one.join("a"), one.join("b")).unwrap();
        let h = measure_home(&one, NodeKind::Profile, limits, &|| false).unwrap();
        assert!(h.bytes < 2 * MIB as u64, "{}", h.bytes);
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("a\nb\u{7f}c", 128), "a\u{FFFD}b\u{FFFD}c");
        assert_eq!(clean_name(&"x".repeat(300), 128).chars().count(), 128);
        let t = vdf_tokens(r#""path" "C:\\Games\\Steam" "name" "A \"quoted\" game""#);
        assert_eq!(t, ["path", "C:\\Games\\Steam", "name", "A \"quoted\" game"]);
    }

    fn mkgen(n: u32, current: bool, booted: bool, paths: &[&str]) -> GenFacts {
        GenFacts {
            generation: Some(n),
            date: i64::from(n) * 100,
            current,
            booted,
            closure: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn system_apps_shared_and_estimates() {
        let mut facts = StoreFacts::default();
        for (p, s) in [
            ("glibc", 100),
            ("sys1", 10),
            ("sys2", 20),
            ("sys3", 30),
            ("old", 5),
            ("mesa", 40),
            ("steam", 1000),
            ("yt", 50),
            ("yt-old", 7),
            ("garbage", 9),
        ] {
            facts.sizes.insert(p.into(), s);
            if p != "garbage" {
                facts.live.insert(p.into());
            }
        }
        // Generation 1 holds "old" only (with glibc); 3 is current, 2 booted.
        let system = vec![
            mkgen(3, true, false, &["glibc", "sys3", "mesa"]),
            mkgen(2, false, true, &["glibc", "sys2", "mesa"]),
            mkgen(1, false, false, &["glibc", "sys1", "old"]),
        ];
        let owners = vec![
            OwnerFacts {
                id: "p1".into(),
                gens: vec![
                    mkgen(2, true, false, &["steam", "yt", "glibc"]),
                    mkgen(1, false, false, &["yt-old", "yt", "glibc"]),
                ],
            },
            OwnerFacts {
                id: "p2".into(),
                gens: vec![mkgen(1, true, false, &["yt", "mesa"])],
            },
            OwnerFacts {
                id: "guest".into(),
                gens: vec![GenFacts {
                    closure: vec!["yt".into()],
                    ..Default::default()
                }],
            },
        ];
        let mut t = store_tree(&facts, &system, &owners);
        sum_ok(&t.system);
        sum_ok(&t.apps);
        assert_eq!(t.garbage_bytes, 9);
        assert_eq!(t.system.bytes, 100 + 10 + 20 + 30 + 5 + 40);
        let gens: Vec<(u32, u64)> = t
            .system
            .children
            .iter()
            .filter(|c| c.kind == NodeKind::Generation)
            .map(|c| (c.generation.unwrap(), c.bytes))
            .collect();
        assert_eq!(gens, [(3, 30), (2, 20), (1, 15)]);
        assert_eq!(t.system.children.last().unwrap().kind, NodeKind::Common);
        assert_eq!(t.system.children.last().unwrap().bytes, 140);
        // Apps: glibc and mesa are the system's; yt is in three envs (shared).
        assert_eq!(t.apps.bytes, 1000 + 7 + 50);
        let p1 = t
            .apps
            .children
            .iter()
            .find(|c| c.id.as_deref() == Some("p1"))
            .unwrap();
        assert_eq!(p1.bytes, 1007);
        let p1_gens: Vec<(u32, u64)> = p1
            .children
            .iter()
            .map(|c| (c.generation.unwrap(), c.bytes))
            .collect();
        assert_eq!(p1_gens, [(2, 1000), (1, 7)]);
        let p2 = t
            .apps
            .children
            .iter()
            .find(|c| c.id.as_deref() == Some("p2"))
            .unwrap();
        assert_eq!(p2.bytes, 0);
        let shared = t.apps.children.last().unwrap();
        assert_eq!((shared.kind, shared.bytes), (NodeKind::Shared, 50));

        // keep 1: generation 1 goes (3 current, 2 booted stay): sys1 + old + garbage.
        let s1 = t.estimates.system[0];
        assert_eq!((s1.keep, s1.generations, s1.bytes), (1, 1, 10 + 5 + 9));
        // keep 3: nothing but garbage.
        assert_eq!(t.estimates.system[2].generations, 0);
        assert_eq!(t.estimates.system[2].bytes, 9);
        assert_eq!(t.estimates.system.len(), 10);
        // Profiles keep 1: p1's generation 1 goes: yt-old (yt stays: in the kept envs).
        let p = t.estimates.profiles[0];
        assert_eq!((p.generations, p.bytes), (1, 7 + 9));
        assert_eq!(t.estimates.profiles[1].generations, 0);
        assert_eq!(removed_by(&system, 1), [1]);
        assert_eq!(removed_by(&system, 2), [1]);

        // The top level: other is what is left of the used space; children add up.
        let disk = DiskUsage {
            total_bytes: 10_000,
            used_bytes: 4_000,
            free_bytes: 5_500,
        };
        let mut home = StorageNode::new(NodeKind::Profile, 2_000);
        home.id = Some("p1".into());
        home.children.push(StorageNode::new(NodeKind::Files, 2_000));
        let empty = {
            let mut n = StorageNode::new(NodeKind::Profile, 0);
            n.id = Some("p2".into());
            n
        };
        let top = top_level(disk, &mut t, &mut [(home, true), (empty, true)], None);
        assert_eq!(top.bytes, 10_000);
        let kinds: Vec<NodeKind> = top.children.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                NodeKind::System,
                NodeKind::Apps,
                NodeKind::Profile,
                NodeKind::Profile,
                NodeKind::Other,
                NodeKind::Free
            ]
        );
        assert!(
            top.children[2].children.is_empty(),
            "no names at the top level"
        );
        assert_eq!(top.children[4].bytes, 10_000 - 5_500 - 205 - 1057 - 2_000);
    }

    const BTRFS: &str = "\
22 1 0:32 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,subvol=/root
23 22 0:33 /nix /nix rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,subvol=/nix
24 23 0:33 /nix/store /nix/store ro,relatime shared:3 - btrfs /dev/nvme0n1p2 rw,subvol=/nix
25 22 0:34 /home /home rw,relatime shared:4 - btrfs /dev/nvme0n1p2 rw,subvol=/home
26 22 0:35 /srv /srv rw,relatime shared:5 - btrfs /dev/nvme0n1p2 rw,subvol=/srv
27 22 259:1 / /boot rw,relatime shared:6 - vfat /dev/nvme0n1p1 rw
28 22 0:27 / /tmp rw shared:7 - tmpfs tmpfs rw
29 22 0:28 / /run rw shared:8 - tmpfs tmpfs rw
30 26 8:17 / /srv/usb\\040disk rw master:1 opt:2 - ext4 /dev/sdb1 rw
";

    fn fs(p: &str) -> Option<String> {
        filesystem_in(BTRFS, Path::new(p))
    }

    #[test]
    fn used_and_free_add_up() {
        // Blocks held back for metadata count as used, as in the ring.
        let u = usage(500, 296);
        assert_eq!((u.total_bytes, u.used_bytes, u.free_bytes), (500, 204, 296));
        assert_eq!(usage(10, 20).used_bytes, 0);
        let real = disk_usage(Path::new("/")).unwrap();
        assert_eq!(real.used_bytes + real.free_bytes, real.total_bytes);
    }

    #[test]
    fn subvolumes_are_one_filesystem() {
        let disk = Some("/dev/nvme0n1p2".to_owned());
        for p in [
            "/",
            "/nix/store",
            "/home/hinteros-p1",
            "/srv/media",
            "/var/lib",
        ] {
            assert_eq!(fs(p), disk, "{p}");
        }
        // Whole path components, tags before the dash, escapes, the last of equal mount points.
        assert_eq!(fs("/boot"), Some("/dev/nvme0n1p1".to_owned()));
        assert_eq!(fs("/bootleg"), disk);
        assert_eq!(fs("/srv/usb disk/x"), Some("/dev/sdb1".to_owned()));
        // tmpfs is no device: two of them are two filesystems.
        assert_eq!(fs("/tmp/x"), Some("dev:0:27".to_owned()));
        assert_eq!(fs("/run/x"), Some("dev:0:28".to_owned()));
        let over = format!("{BTRFS}31 22 8:33 / /home rw - ext4 /dev/sdc1 rw\n");
        assert_eq!(
            filesystem_in(&over, Path::new("/home/a")),
            Some("/dev/sdc1".to_owned())
        );
        assert_eq!(filesystem_in("nonsense\n", Path::new("/")), None);
        assert_eq!(filesystem_in("", Path::new("/")), None);
    }

    #[test]
    fn the_real_mounts_answer() {
        let d = tempfile::tempdir().unwrap();
        let id = filesystem_of(d.path()).unwrap();
        assert_eq!(filesystem_of(&d.path().join(".")), Some(id));
        assert_eq!(filesystem_of(&d.path().join("gone")), None);
    }

    #[test]
    fn media_is_a_total() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("media/Movies/film"), 3 * MIB);
        write(&d.path().join("backups/x"), MIB);
        let m = measure_media(d.path(), small(), &|| false).unwrap();
        assert_eq!(m.kind, NodeKind::Media);
        assert!(m.bytes >= 4 * MIB as u64, "{}", m.bytes);
        assert!(m.children.is_empty() && m.name.is_none());
        assert!(measure_media(&d.path().join("gone"), small(), &|| false).is_none());
    }

    fn deep(kind: NodeKind, bytes: &[u64]) -> StorageNode {
        let mut n = StorageNode::new(kind, bytes.iter().sum());
        for b in bytes {
            let mut c = StorageNode::new(NodeKind::Dir, *b);
            c.children.push(StorageNode::new(NodeKind::Files, b / 3));
            c.children
                .push(StorageNode::new(NodeKind::Files, b - b / 3));
            n.children.push(c);
        }
        n
    }

    fn store(system: StorageNode, apps: StorageNode) -> StoreTree {
        StoreTree {
            system,
            apps,
            garbage_bytes: 0,
            estimates: CleanupEstimates::default(),
        }
    }

    fn of_kind(top: &StorageNode, kind: NodeKind) -> Vec<u64> {
        let found = top.children.iter().filter(|c| c.kind == kind);
        found.map(|c| c.bytes).collect()
    }

    #[test]
    fn compression_scales_the_known_segments_to_the_disk() {
        // 12 000 known, 6 000 used: half of everything.
        let disk = DiskUsage {
            total_bytes: 10_000,
            used_bytes: 5_900,
            free_bytes: 4_000,
        };
        let mut s = store(
            deep(NodeKind::System, &[3_001, 999]),
            deep(NodeKind::Apps, &[1_000, 1_000]),
        );
        let mut homes = [
            (deep(NodeKind::Profile, &[2_000, 1_000]), true),
            (deep(NodeKind::Guest, &[1_001, 1]), true),
            // Another disk: as it is, and none of this one's used space.
            (deep(NodeKind::Profile, &[700]), false),
        ];
        s.garbage_bytes = 600;
        s.estimates.system.push(CleanupEstimate {
            keep: 1,
            generations: 2,
            bytes: 1_200,
        });
        let media = (StorageNode::new(NodeKind::Media, 1_998), true);
        let top = top_level(disk, &mut s, &mut homes, Some(media));
        // The store got half: so do its garbage and estimates.
        assert_eq!(s.garbage_bytes, 300);
        assert_eq!(s.estimates.system[0].bytes, 600);
        sum_ok(&top);
        for c in &top.children {
            sum_ok(c);
        }
        for (h, _) in &homes {
            sum_ok(h);
        }
        assert_eq!(of_kind(&top, NodeKind::Other), Vec::<u64>::new());
        assert_eq!(of_kind(&top, NodeKind::System), [2_000]);
        assert_eq!(of_kind(&top, NodeKind::Apps), [1_000]);
        assert_eq!(of_kind(&top, NodeKind::Media), [999]);
        assert_eq!(of_kind(&top, NodeKind::Guest), [501]);
        assert_eq!(of_kind(&top, NodeKind::Profile), [1_500, 700]);
        assert_eq!(top.bytes, 10_000 + 700);
        // The trees agree with the top level, and the other disk's home is untouched.
        assert_eq!(homes[0].0.bytes, 1_500);
        assert_eq!(homes[2].0.bytes, 700);
        assert_eq!(homes[2].0.children[0].bytes, 700);
    }

    #[test]
    fn nothing_changes_while_the_known_fits() {
        let disk = DiskUsage {
            total_bytes: 10_000,
            used_bytes: 6_000,
            free_bytes: 4_000,
        };
        let mut s = store(
            deep(NodeKind::System, &[3_000]),
            deep(NodeKind::Apps, &[1_000]),
        );
        // Exactly the used space.
        let mut homes = [(deep(NodeKind::Profile, &[1_500]), true)];
        let media = (StorageNode::new(NodeKind::Media, 500), true);
        let top = top_level(disk, &mut s, &mut homes, Some(media));
        assert_eq!(of_kind(&top, NodeKind::System), [3_000]);
        assert_eq!(of_kind(&top, NodeKind::Media), [500]);
        assert!(of_kind(&top, NodeKind::Other).is_empty());
        assert_eq!(top.children[0], s.system);
        // Below it: other is the rest. An empty or absent `/srv` is no segment.
        let media = (StorageNode::new(NodeKind::Media, 0), true);
        let top = top_level(disk, &mut s, &mut homes, Some(media));
        assert_eq!(of_kind(&top, NodeKind::Other), [500]);
        assert!(of_kind(&top, NodeKind::Media).is_empty());
        assert_eq!(top.bytes, 10_000);
    }

    #[test]
    fn shares_add_up_exactly() {
        assert_eq!(shares(&[1, 1, 1], 2), [1, 1, 0]);
        assert_eq!(shares(&[3, 1], 2), [2, 0]);
        assert_eq!(shares(&[0, 0], 0), [0, 0]);
        assert_eq!(shares(&[], 0), Vec::<u64>::new());
        let big = [u64::MAX / 2, u64::MAX / 3, 7];
        assert_eq!(shares(&big, 1 << 40).iter().sum::<u64>(), 1 << 40);
        // A child scaled to nothing goes; what is left still adds up.
        let mut n = deep(NodeKind::System, &[1_000, 1]);
        scale(&mut n, 10);
        sum_ok(&n);
        assert_eq!(n.children.len(), 1);
    }
}

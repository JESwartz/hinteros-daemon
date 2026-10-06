//! Icons from a Firefox profile: the favicon Firefox itself stored when the app's kiosk
//! profile visited the site. Used when the site refuses to hand its icon to a non-browser
//! (Crunchyroll answers 403).
//!
//! Firefox keeps favicons in `<profile>/favicons.sqlite` (tables `moz_icons`,
//! `moz_pages_w_icons`, `moz_icons_to_pages`). Firefox may have the database open, in WAL
//! mode, so it is never opened in place: the file and its `-wal` are copied to a private
//! temporary directory and the copy is read (SQLite replays the copied WAL, so the newest
//! icons are seen). The largest image stored for the app's site wins.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use url::Url;

use super::web::{self, Image};

/// Icons per site we look at before giving up on unusable data.
const MAX_ROWS: usize = 16;

/// The best favicon Firefox stored for `site_url`, if any. `Ok(None)`: no profile database
/// yet, or no icon for this site.
pub fn favicon(profile: &Path, site_url: &str) -> Result<Option<Image>> {
    let db = profile.join("favicons.sqlite");
    if !db.is_file() {
        return Ok(None);
    }
    let site = Url::parse(site_url).with_context(|| format!("site_url {site_url}"))?;
    let Some(host) = site.host_str() else {
        return Ok(None);
    };
    let prefix = format!("{}://{host}", site.scheme());

    let tmp = tempfile::tempdir().context("temporary directory")?;
    let copy = tmp.path().join("favicons.sqlite");
    fs::copy(&db, &copy).with_context(|| format!("copying {}", db.display()))?;
    let wal = profile.join("favicons.sqlite-wal");
    if wal.is_file() {
        // Best effort: Firefox may checkpoint (and delete) it right now.
        let _ = fs::copy(&wal, tmp.path().join("favicons.sqlite-wal"));
    }
    let conn = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening a copy of {}", db.display()))?;
    best_icon(&conn, &prefix, site_url)
}

fn best_icon(conn: &Connection, prefix: &str, page: &str) -> Result<Option<Image>> {
    // Icons linked to a page of the site, or the site's root icon (`/favicon.ico`). Exact
    // page matches first, then the largest. SVGs are stored with width 65535.
    let mut stmt = conn.prepare(
        "SELECT i.data FROM moz_icons i
           LEFT JOIN moz_icons_to_pages ip ON ip.icon_id = i.id
           LEFT JOIN moz_pages_w_icons p ON p.id = ip.page_id
          WHERE i.data IS NOT NULL AND length(i.data) > 0 AND length(i.data) <= ?3
            AND (substr(p.page_url, 1, length(?1) + 1) = ?1 || '/'
                 OR substr(i.icon_url, 1, length(?1) + 1) = ?1 || '/')
          GROUP BY i.id
          ORDER BY max(p.page_url = ?2) DESC, i.width DESC, length(i.data) DESC
          LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![prefix, page, web::MAX_BYTES as i64, MAX_ROWS as i64],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    for bytes in rows {
        let bytes = bytes?;
        if let Some(ext) = web::sniff(&bytes) {
            return Ok(Some(Image { bytes, ext }));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\nbig";

    /// A profile with the subset of Firefox's favicon schema we read.
    pub(crate) fn profile(rows: &[(&str, &str, i64, &[u8])]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let conn = Connection::open(t.path().join("favicons.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_icons (id INTEGER PRIMARY KEY, icon_url TEXT NOT NULL,
               fixed_icon_url_hash INTEGER NOT NULL, width INTEGER NOT NULL DEFAULT 0,
               root INTEGER NOT NULL DEFAULT 0, color INTEGER, expire_ms INTEGER NOT NULL DEFAULT 0,
               data BLOB);
             CREATE TABLE moz_pages_w_icons (id INTEGER PRIMARY KEY, page_url TEXT NOT NULL,
               page_url_hash INTEGER NOT NULL);
             CREATE TABLE moz_icons_to_pages (page_id INTEGER NOT NULL, icon_id INTEGER NOT NULL,
               expire_ms INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (page_id, icon_id));",
        )
        .unwrap();
        for (n, (page, icon_url, width, data)) in rows.iter().enumerate() {
            let n = n as i64 + 1;
            conn.execute(
                "INSERT INTO moz_icons (id, icon_url, fixed_icon_url_hash, width, data)
                 VALUES (?1, ?2, 0, ?3, ?4)",
                params![n, icon_url, width, data],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO moz_pages_w_icons (id, page_url, page_url_hash) VALUES (?1, ?2, 0)",
                params![n, page],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO moz_icons_to_pages (page_id, icon_id) VALUES (?1, ?1)",
                params![n],
            )
            .unwrap();
        }
        t
    }

    #[test]
    fn picks_the_largest_icon_of_the_site() {
        let t = profile(&[
            (
                "https://other.test/",
                "https://other.test/o.png",
                512,
                b"\x89PNG\r\n\x1a\nother",
            ),
            (
                "https://www.crunchyroll.com/",
                "https://x/16.png",
                16,
                b"\x89PNG\r\n\x1a\nsmall",
            ),
            (
                "https://www.crunchyroll.com/",
                "https://x/180.png",
                180,
                PNG,
            ),
            (
                "https://www.crunchyroll.com/watch",
                "https://x/bad",
                400,
                b"not an image",
            ),
        ]);
        let img = favicon(t.path(), "https://www.crunchyroll.com/")
            .unwrap()
            .unwrap();
        assert_eq!((img.ext, img.bytes.as_slice()), ("png", PNG));
        // Another site of the profile is not this one.
        assert_eq!(favicon(t.path(), "https://nope.test/").unwrap(), None);
    }

    #[test]
    fn prefers_the_exact_page_then_size() {
        let t = profile(&[
            (
                "https://a.test/tv",
                "https://a.test/s.svg",
                65535,
                b"<svg xmlns=''/>",
            ),
            (
                "https://a.test/",
                "https://a.test/f.png",
                32,
                b"\x89PNG\r\n\x1a\nroot",
            ),
        ]);
        let svg = favicon(t.path(), "https://a.test/tv").unwrap().unwrap();
        assert_eq!(svg.ext, "svg");
        let png = favicon(t.path(), "https://a.test/").unwrap().unwrap();
        assert_eq!(png.ext, "png");
    }

    #[test]
    fn missing_profile_or_database_is_no_icon() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(favicon(t.path(), "https://a.test/").unwrap(), None);
        assert_eq!(
            favicon(&t.path().join("gone"), "https://a.test/").unwrap(),
            None
        );
    }

    #[test]
    fn reads_icons_still_in_the_wal_and_leaves_the_profile_alone() {
        // Firefox holds the database open in WAL mode: recent icons only live in the -wal.
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("favicons.sqlite");
        let live = Connection::open(&path).unwrap();
        live.pragma_update(None, "journal_mode", "WAL").unwrap();
        live.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        live.execute_batch(
            "CREATE TABLE moz_icons (id INTEGER PRIMARY KEY, icon_url TEXT, width INTEGER, data BLOB);
             CREATE TABLE moz_pages_w_icons (id INTEGER PRIMARY KEY, page_url TEXT);
             CREATE TABLE moz_icons_to_pages (page_id INTEGER, icon_id INTEGER);
             INSERT INTO moz_pages_w_icons VALUES (1, 'https://a.test/');
             INSERT INTO moz_icons_to_pages VALUES (1, 1);",
        )
        .unwrap();
        live.execute(
            "INSERT INTO moz_icons VALUES (1, 'https://a.test/i.png', 64, ?1)",
            params![PNG],
        )
        .unwrap();
        assert!(t.path().join("favicons.sqlite-wal").exists());
        let before = fs::read_dir(t.path()).unwrap().count();
        let img = favicon(t.path(), "https://a.test/").unwrap().unwrap();
        assert_eq!(img.bytes, PNG);
        // Nothing was created in (or written to) the profile; the connection still works.
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), before);
        live.execute("DELETE FROM moz_icons", []).unwrap();
    }
}

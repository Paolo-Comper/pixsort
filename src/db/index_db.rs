//! Database A — the operational image index.
//!
//! This is a cache, not archive metadata. It can be deleted and rebuilt at any
//! time (except where a pending plan references its rows, which the UI warns
//! about). It is designed for resumable scans: every file is inserted as
//! `pending` before work starts, and updated after it finishes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::model::{DateSource, ExifInfo, ImageFormatKind, PhotoRecord, PhotoStatus, RootKind};

pub const SCHEMA_VERSION: i64 = 1;

pub struct IndexDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct PendingFile {
    pub path: String,
    pub root: RootKind,
    pub file_size: u64,
    pub mtime_ns: i64,
    pub format: ImageFormatKind,
}

#[derive(Debug, Clone)]
pub struct CachedFile {
    pub file_size: u64,
    pub mtime_ns: i64,
    pub status: PhotoStatus,
}

const SELECT_COLUMNS: &str = "id, path, root, file_size, mtime_ns, format, width, height, \
     pixel_hash, phash, taken_on, taken_at_ns, taken_source, orientation, is_animated, \
     exif_json, status, error, indexed_at";

impl IndexDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open image index {}", path.display()))?;
        configure(&conn)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("cannot open image index {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        Ok(Self { conn })
    }

    pub fn integrity_check(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result != "ok" {
            bail!("image index failed SQLite quick_check: {result}");
        }
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS photos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                root TEXT NOT NULL,
                file_size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                format TEXT NOT NULL,
                width INTEGER NOT NULL DEFAULT 0,
                height INTEGER NOT NULL DEFAULT 0,
                pixel_hash TEXT,
                phash INTEGER,
                taken_on TEXT,
                taken_at_ns INTEGER,
                taken_source TEXT NOT NULL DEFAULT 'unknown',
                orientation INTEGER NOT NULL DEFAULT 1,
                is_animated INTEGER NOT NULL DEFAULT 0,
                exif_json TEXT NOT NULL DEFAULT '{}',
                status TEXT NOT NULL,
                error TEXT,
                indexed_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_photos_root ON photos(root);
            CREATE INDEX IF NOT EXISTS idx_photos_pixel_hash ON photos(pixel_hash);
            CREATE INDEX IF NOT EXISTS idx_photos_phash ON photos(phash);
            CREATE INDEX IF NOT EXISTS idx_photos_status ON photos(status);
            "#,
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Returns size/mtime/status for every known file under a root.
    pub fn cache_map(&self, root: RootKind) -> Result<HashMap<String, CachedFile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, file_size, mtime_ns, status FROM photos WHERE root=?1")?;
        let rows = stmt.query_map([root.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                CachedFile {
                    file_size: row.get::<_, i64>(1)? as u64,
                    mtime_ns: row.get(2)?,
                    status: PhotoStatus::from_str(&row.get::<_, String>(3)?)
                        .unwrap_or(PhotoStatus::Pending),
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, cached) = row?;
            out.insert(path, cached);
        }
        Ok(out)
    }

    pub fn upsert_pending(&mut self, file: &PendingFile) -> Result<()> {
        let now = now_ns();
        self.conn.execute(
            r#"
            INSERT INTO photos
                (path, root, file_size, mtime_ns, format, width, height,
                 status, error, indexed_at, exif_json)
            VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 'pending', NULL, ?6, '{}')
            ON CONFLICT(path) DO UPDATE SET
                root=excluded.root,
                file_size=excluded.file_size,
                mtime_ns=excluded.mtime_ns,
                format=excluded.format,
                width=0,
                height=0,
                pixel_hash=NULL,
                phash=NULL,
                taken_on=NULL,
                taken_at_ns=NULL,
                taken_source='unknown',
                orientation=1,
                is_animated=0,
                exif_json='{}',
                status='pending',
                error=NULL,
                indexed_at=excluded.indexed_at
            "#,
            params![
                file.path,
                file.root.as_str(),
                file.file_size as i64,
                file.mtime_ns,
                file.format.as_str(),
                now
            ],
        )?;
        Ok(())
    }

    pub fn upsert_pending_batch(&mut self, files: &[PendingFile]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let now = now_ns();
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO photos
                    (path, root, file_size, mtime_ns, format, width, height,
                     status, error, indexed_at, exif_json)
                VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 'pending', NULL, ?6, '{}')
                ON CONFLICT(path) DO UPDATE SET
                    root=excluded.root,
                    file_size=excluded.file_size,
                    mtime_ns=excluded.mtime_ns,
                    format=excluded.format,
                    width=0, height=0,
                    pixel_hash=NULL, phash=NULL,
                    taken_on=NULL, taken_at_ns=NULL, taken_source='unknown',
                    orientation=1, is_animated=0, exif_json='{}',
                    status='pending', error=NULL,
                    indexed_at=excluded.indexed_at
                "#,
            )?;
            for file in files {
                stmt.execute(params![
                    file.path,
                    file.root.as_str(),
                    file.file_size as i64,
                    file.mtime_ns,
                    file.format.as_str(),
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn update_indexed(&mut self, photo: &PhotoRecord) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_indexed(&tx, photo)?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_indexed_batch(&mut self, photos: &[PhotoRecord]) -> Result<()> {
        if photos.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                r#"
                UPDATE photos SET
                    path=?1, root=?2, file_size=?3, mtime_ns=?4, format=?5,
                    width=?6, height=?7, pixel_hash=?8, phash=?9,
                    taken_on=?10, taken_at_ns=?11, taken_source=?12,
                    orientation=?13, is_animated=?14, exif_json=?15,
                    status='indexed', error=NULL, indexed_at=?16
                WHERE path=?17
                "#,
            )?;
            for photo in photos {
                let updated = stmt.execute(params![
                    photo.path,
                    photo.root.as_str(),
                    photo.file_size as i64,
                    photo.mtime_ns,
                    photo.format.as_str(),
                    photo.width as i64,
                    photo.height as i64,
                    photo.pixel_hash,
                    photo.phash.map(|v| v as i64),
                    photo.taken_on,
                    photo.taken_at_ns,
                    photo.taken_source.as_str(),
                    photo.orientation as i64,
                    photo.is_animated as i64,
                    serde_json::to_string(&photo.exif)?,
                    photo.indexed_at,
                    photo.path
                ])?;
                if updated == 0 {
                    insert_full(&tx, photo)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn fail_batch(&mut self, failures: &[(String, String)]) -> Result<()> {
        if failures.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE photos SET status='failed', error=?1, indexed_at=?2 WHERE path=?3",
            )?;
            let now = now_ns();
            for (path, error) in failures {
                stmt.execute(params![error, now, path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_path(&mut self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM photos WHERE path=?1", [path])?;
        Ok(())
    }

    pub fn delete_paths(&mut self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut count = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM photos WHERE path=?1")?;
            for path in paths {
                count += stmt.execute([path])?;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn get_by_path(&self, path: &str) -> Result<Option<PhotoRecord>> {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM photos WHERE path=?1");
        Ok(self.conn.query_row(&sql, [path], map_photo).optional()?)
    }

    pub fn get_by_id(&self, id: i64) -> Result<Option<PhotoRecord>> {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM photos WHERE id=?1");
        Ok(self.conn.query_row(&sql, [id], map_photo).optional()?)
    }

    pub fn load_indexed(&self, root: Option<RootKind>) -> Result<Vec<PhotoRecord>> {
        let (sql, param): (String, Option<&str>) = match root {
            Some(root) => (
                format!(
                    "SELECT {SELECT_COLUMNS} FROM photos \
                     WHERE status='indexed' AND root=?1 ORDER BY path"
                ),
                Some(root.as_str()),
            ),
            None => (
                format!(
                    "SELECT {SELECT_COLUMNS} FROM photos \
                     WHERE status='indexed' ORDER BY path"
                ),
                None,
            ),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(root) = param {
            stmt.query_map([root], map_photo)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], map_photo)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    pub fn load_all(&self, root: Option<RootKind>) -> Result<Vec<PhotoRecord>> {
        let sql = match root {
            Some(_) => format!("SELECT {SELECT_COLUMNS} FROM photos WHERE root=?1 ORDER BY path"),
            None => format!("SELECT {SELECT_COLUMNS} FROM photos ORDER BY path"),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(root) = root {
            stmt.query_map([root.as_str()], map_photo)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], map_photo)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    pub fn paths_for_root(&self, root: RootKind) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM photos WHERE root=?1")?;
        let rows = stmt.query_map([root.as_str()], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn stats(&self) -> Result<IndexStats> {
        let mut out = IndexStats::default();
        let mut stmt = self
            .conn
            .prepare("SELECT root, status, COUNT(*) FROM photos GROUP BY root, status")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        for row in rows {
            let (root, status, count) = row?;
            match root.as_str() {
                "archive" => out.archive_total += count,
                "input" => out.input_total += count,
                _ => {}
            }
            match status.as_str() {
                "indexed" => out.indexed += count,
                "failed" => out.failed += count,
                "pending" => out.pending += count,
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn count_indexed(&self, root: RootKind) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE root=?1 AND status='indexed'",
            [root.as_str()],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    /// Removes index rows for files that no longer exist under `root`.
    pub fn remove_missing(&mut self, root: RootKind, seen: &HashSet<String>) -> Result<usize> {
        let existing = self.paths_for_root(root)?;
        let missing: Vec<String> = existing.into_iter().filter(|p| !seen.contains(p)).collect();
        self.delete_paths(&missing)
    }

    /// Moves an indexed row to a new path/root after a successful file
    /// operation. `photo` must describe the file at the new location.
    pub fn relocate(&mut self, old_path: &str, photo: &PhotoRecord) -> Result<()> {
        let tx = self.conn.transaction()?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM photos WHERE path=?1 AND path<>?2",
                params![photo.path, old_path],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if exists {
            bail!("image index already contains {}", photo.path);
        }
        let updated = tx.execute(
            "UPDATE photos SET path=?1, root=?2, file_size=?3, mtime_ns=?4, format=?5,
             status='indexed', error=NULL, indexed_at=?6 WHERE path=?7",
            params![
                photo.path,
                photo.root.as_str(),
                photo.file_size as i64,
                photo.mtime_ns,
                photo.format.as_str(),
                now_ns(),
                old_path
            ],
        )?;
        if updated == 0 {
            insert_full(&tx, photo)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub archive_total: u64,
    pub input_total: u64,
    pub indexed: u64,
    pub failed: u64,
    pub pending: u64,
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // WAL improves read/write concurrency between the GUI and a scan job.
    // In-memory databases cannot use WAL; the pragma simply returns "memory".
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    Ok(())
}

fn write_indexed(conn: &Connection, photo: &PhotoRecord) -> Result<()> {
    let updated = conn.execute(
        r#"
        UPDATE photos SET
            path=?1, root=?2, file_size=?3, mtime_ns=?4, format=?5,
            width=?6, height=?7, pixel_hash=?8, phash=?9,
            taken_on=?10, taken_at_ns=?11, taken_source=?12,
            orientation=?13, is_animated=?14, exif_json=?15,
            status='indexed', error=NULL, indexed_at=?16
        WHERE path=?1
        "#,
        params![
            photo.path,
            photo.root.as_str(),
            photo.file_size as i64,
            photo.mtime_ns,
            photo.format.as_str(),
            photo.width as i64,
            photo.height as i64,
            photo.pixel_hash,
            photo.phash.map(|v| v as i64),
            photo.taken_on,
            photo.taken_at_ns,
            photo.taken_source.as_str(),
            photo.orientation as i64,
            photo.is_animated as i64,
            serde_json::to_string(&photo.exif)?,
            photo.indexed_at,
        ],
    )?;
    if updated == 0 {
        insert_full(conn, photo)?;
    }
    Ok(())
}

fn insert_full(conn: &Connection, photo: &PhotoRecord) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO photos
            (path, root, file_size, mtime_ns, format, width, height,
             pixel_hash, phash, taken_on, taken_at_ns, taken_source,
             orientation, is_animated, exif_json, status, error, indexed_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, 'indexed', NULL, ?16)
        "#,
        params![
            photo.path,
            photo.root.as_str(),
            photo.file_size as i64,
            photo.mtime_ns,
            photo.format.as_str(),
            photo.width as i64,
            photo.height as i64,
            photo.pixel_hash,
            photo.phash.map(|v| v as i64),
            photo.taken_on,
            photo.taken_at_ns,
            photo.taken_source.as_str(),
            photo.orientation as i64,
            photo.is_animated as i64,
            serde_json::to_string(&photo.exif)?,
            photo.indexed_at,
        ],
    )?;
    Ok(())
}

fn map_photo(row: &rusqlite::Row<'_>) -> rusqlite::Result<PhotoRecord> {
    let exif_json: String = row.get(15)?;
    let exif: ExifInfo = serde_json::from_str(&exif_json).unwrap_or_default();
    let taken_source = DateSource::from_str(&row.get::<_, String>(12)?);
    Ok(PhotoRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        root: RootKind::from_str(&row.get::<_, String>(2)?).unwrap_or(RootKind::Archive),
        file_size: row.get::<_, i64>(3)? as u64,
        mtime_ns: row.get(4)?,
        format: ImageFormatKind::from_str(&row.get::<_, String>(5)?)
            .unwrap_or(ImageFormatKind::Jpeg),
        width: row.get::<_, i64>(6)? as u32,
        height: row.get::<_, i64>(7)? as u32,
        pixel_hash: row.get(8)?,
        phash: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        taken_on: row.get(10)?,
        taken_at_ns: row.get(11)?,
        taken_source,
        orientation: row.get::<_, i64>(13)? as u16,
        is_animated: row.get::<_, i64>(14)? != 0,
        exif,
        status: PhotoStatus::from_str(&row.get::<_, String>(16)?).unwrap_or(PhotoStatus::Failed),
        error: row.get(17)?,
        indexed_at: row.get(18)?,
    })
}

pub fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DateSource;

    fn photo(path: &str) -> PhotoRecord {
        PhotoRecord {
            id: 0,
            path: path.into(),
            root: RootKind::Archive,
            file_size: 100,
            mtime_ns: 1_600_000_000_000_000_000,
            format: ImageFormatKind::Jpeg,
            width: 100,
            height: 80,
            pixel_hash: Some("abc".into()),
            phash: Some(42),
            taken_on: Some("2020-09-13".into()),
            taken_at_ns: Some(1_600_000_000_000_000_000),
            taken_source: DateSource::ExifOriginal,
            orientation: 6,
            is_animated: false,
            exif: ExifInfo::default(),
            status: PhotoStatus::Indexed,
            error: None,
            indexed_at: 1,
        }
    }

    #[test]
    fn pending_then_indexed_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = IndexDb::open(&dir.path().join("index.sqlite")).unwrap();
        db.upsert_pending(&PendingFile {
            path: "/photos/a.jpg".into(),
            root: RootKind::Archive,
            file_size: 100,
            mtime_ns: 1_600_000_000_000_000_000,
            format: ImageFormatKind::Jpeg,
        })
        .unwrap();
        assert_eq!(db.cache_map(RootKind::Archive).unwrap().len(), 1);
        let mut p = photo("/photos/a.jpg");
        p.status = PhotoStatus::Indexed;
        db.update_indexed(&p).unwrap();
        let loaded = db.get_by_path("/photos/a.jpg").unwrap().unwrap();
        assert_eq!(loaded.pixel_hash.as_deref(), Some("abc"));
        assert_eq!(loaded.phash, Some(42));
        assert_eq!(loaded.taken_source, DateSource::ExifOriginal);
        assert_eq!(loaded.orientation, 6);
        db.integrity_check().unwrap();
    }

    #[test]
    fn cache_map_detects_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = IndexDb::open(&dir.path().join("index.sqlite")).unwrap();
        let mut p = photo("/photos/a.jpg");
        p.status = PhotoStatus::Indexed;
        db.update_indexed(&p).unwrap();
        let cached = db.cache_map(RootKind::Archive).unwrap();
        assert_eq!(cached["/photos/a.jpg"].file_size, 100);
        assert_eq!(cached["/photos/a.jpg"].status, PhotoStatus::Indexed);
    }

    #[test]
    fn corrupt_database_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();
        assert!(IndexDb::open(&path).is_err());
        let read_only = IndexDb::open_read_only(&path).unwrap();
        assert!(read_only.integrity_check().is_err());
        assert!(read_only.stats().is_err());
    }

    #[test]
    fn relocated_rows_keep_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = IndexDb::open(&dir.path().join("index.sqlite")).unwrap();
        let p = photo("/in/a.jpg");
        db.update_indexed(&p).unwrap();
        let mut moved = p.clone();
        moved.path = "/archive/2020/a.jpg".into();
        moved.root = RootKind::Archive;
        db.relocate("/in/a.jpg", &moved).unwrap();
        assert!(db.get_by_path("/in/a.jpg").unwrap().is_none());
        assert!(db.get_by_path("/archive/2020/a.jpg").unwrap().is_some());
    }
}

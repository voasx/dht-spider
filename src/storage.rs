use rusqlite::{Connection, params};
use std::sync::Mutex;

pub struct Storage {
    conn: Mutex<Connection>,
}

#[derive(Clone)]
pub struct TorrentEntry {
    pub infohash: String,
    pub name: String,
    pub files: String,
    pub created_at: i64,
}

impl Storage {
    pub fn open(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS torrents (
                 infohash  TEXT PRIMARY KEY,
                 name      TEXT NOT NULL,
                 files     TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
             );
             CREATE INDEX IF NOT EXISTS idx_torrents_created_at ON torrents(created_at);"
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert(&self, infohash: &str, name: &str, files_json: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO torrents (infohash, name, files) VALUES (?1, ?2, ?3)",
            params![infohash, name, files_json],
        )?;
        Ok(())
    }

    pub fn list(&self, page: u32, page_size: u32, search: &str) -> Result<(Vec<TorrentEntry>, u32), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let offset = (page.saturating_sub(1)) * page_size;

        let (entries, total) = if search.is_empty() {
            let total: u32 = conn.query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))?;
            let mut stmt = conn.prepare("SELECT infohash, name, files, created_at FROM torrents ORDER BY created_at DESC LIMIT ?1 OFFSET ?2")?;
            let rows = stmt.query_map(params![page_size, offset], |row| {
                Ok(TorrentEntry {
                    infohash: row.get(0)?,
                    name: row.get(1)?,
                    files: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?;
            let entries: Vec<TorrentEntry> = rows.filter_map(|r| r.ok()).collect();
            (entries, total)
        } else {
            let pattern = format!("%{}%", search.replace('%', "\\%").replace('_', "\\_"));
            let total: u32 = conn.query_row("SELECT COUNT(*) FROM torrents WHERE name LIKE ?1 ESCAPE '\\'", params![pattern], |r| r.get(0))?;
            let mut stmt = conn.prepare("SELECT infohash, name, files, created_at FROM torrents WHERE name LIKE ?1 ESCAPE '\\' ORDER BY created_at DESC LIMIT ?2 OFFSET ?3")?;
            let rows = stmt.query_map(params![pattern, page_size, offset], |row| {
                Ok(TorrentEntry {
                    infohash: row.get(0)?,
                    name: row.get(1)?,
                    files: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?;
            let entries: Vec<TorrentEntry> = rows.filter_map(|r| r.ok()).collect();
            (entries, total)
        };

        Ok((entries, total))
    }

    pub fn count(&self) -> Result<u32, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let total: u32 = conn.query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))?;
        Ok(total)
    }

    pub fn get_by_infohash(&self, infohash: &str) -> Result<Option<TorrentEntry>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT infohash, name, files, created_at FROM torrents WHERE infohash = ?1")?;
        let mut rows = stmt.query(params![infohash])?;
        match rows.next()? {
            Some(row) => Ok(Some(TorrentEntry {
                infohash: row.get(0)?,
                name: row.get(1)?,
                files: row.get(2)?,
                created_at: row.get(3)?,
            })),
            None => Ok(None),
        }
    }
}

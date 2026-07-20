use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sql: {0}")]
    Sql(#[from] rusqlite::Error),
}

pub struct Cache {
    conn: Mutex<Connection>,
    cap_bytes: u64,
    max_ttl: Duration,
}

pub struct Hit {
    pub etag: String,
    pub brotli_body: Vec<u8>,
    pub fetched_at: SystemTime,
    pub max_age: Duration,
}

impl Hit {
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        now.duration_since(self.fetched_at)
            .map(|age| age < self.max_age)
            .unwrap_or(false)
    }
}

pub struct CacheEntry {
    pub url: String,
    pub etag: String,
    pub fetched_at: SystemTime,
    pub last_used: SystemTime,
    pub size: u64,
    pub max_age: Duration,
}

impl Cache {
    pub fn open(dir: &Path, cap_bytes: u64, max_ttl: Duration) -> Result<Self, CacheError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cache.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS entries (
                url         TEXT PRIMARY KEY,
                etag        TEXT NOT NULL,
                brotli_body BLOB NOT NULL,
                fetched_at  INTEGER NOT NULL,
                last_used   INTEGER NOT NULL,
                size        INTEGER NOT NULL,
                max_age     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_last_used ON entries(last_used);
            "#,
        )?;
        Ok(Self { conn: Mutex::new(conn), cap_bytes, max_ttl })
    }

    pub fn lookup(&self, url: &str) -> Option<Hit> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT etag, brotli_body, fetched_at, max_age FROM entries WHERE url = ?1",
            params![url],
            |row| {
                let etag: String = row.get(0)?;
                let body: Vec<u8> = row.get(1)?;
                let fetched_at_secs: i64 = row.get(2)?;
                let max_age_secs: i64 = row.get(3)?;
                Ok((etag, body, fetched_at_secs, max_age_secs))
            },
        )
        .ok()
        .map(|(etag, body, f, m)| Hit {
            etag,
            brotli_body: body,
            fetched_at: UNIX_EPOCH + Duration::from_secs(f.max(0) as u64),
            max_age: Duration::from_secs(m.max(0) as u64),
        })
    }

    pub fn insert(
        &self,
        url: &str,
        etag: &str,
        brotli_body: &[u8],
        server_max_age_secs: i32,
    ) {
        if server_max_age_secs < 0 {
            return;
        }
        let capped = (server_max_age_secs as u64).min(self.max_ttl.as_secs());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let size = brotli_body.len() as i64;
        let cap = self.cap_bytes as i64;

        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cache insert: begin failed: {}", e);
                return;
            }
        };
        let up = tx.execute(
            r#"INSERT INTO entries (url, etag, brotli_body, fetched_at, last_used, size, max_age)
               VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
               ON CONFLICT(url) DO UPDATE SET
                   etag = excluded.etag,
                   brotli_body = excluded.brotli_body,
                   fetched_at = excluded.fetched_at,
                   last_used = excluded.last_used,
                   size = excluded.size,
                   max_age = excluded.max_age"#,
            params![url, etag, brotli_body, now, size, capped as i64],
        );
        if let Err(e) = up {
            tracing::warn!("cache insert: upsert failed: {}", e);
            return;
        }
        // Evict LRU until under cap.
        let total: i64 = tx
            .query_row("SELECT COALESCE(SUM(size), 0) FROM entries", [], |r| r.get(0))
            .unwrap_or(0);
        if total > cap {
            let mut over = total - cap;
            let victims: Vec<(String, i64)> = tx
                .prepare("SELECT url, size FROM entries ORDER BY last_used ASC")
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .collect()
                })
                .unwrap_or_default();
            for (v_url, v_size) in victims {
                if over <= 0 {
                    break;
                }
                if v_url == url {
                    continue; // never evict what we just inserted
                }
                if let Err(e) = tx.execute("DELETE FROM entries WHERE url = ?1", params![v_url]) {
                    tracing::warn!("cache evict: {}", e);
                    continue;
                }
                over -= v_size;
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("cache insert: commit failed: {}", e);
        }
    }

    pub fn touch_fresh(&self, url: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute(
            "UPDATE entries SET fetched_at = ?1, last_used = ?1 WHERE url = ?2",
            params![now, url],
        );
    }

    pub fn touch_last_used(&self, url: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute(
            "UPDATE entries SET last_used = ?1 WHERE url = ?2",
            params![now, url],
        );
    }

    pub fn delete(&self, url: &str) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute("DELETE FROM entries WHERE url = ?1", params![url]);
    }

    pub fn clear(&self) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute("DELETE FROM entries", []);
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    pub fn list(&self) -> Vec<CacheEntry> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT url, etag, fetched_at, last_used, size, max_age FROM entries ORDER BY last_used DESC LIMIT 200",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| {
            Ok(CacheEntry {
                url: row.get(0)?,
                etag: row.get(1)?,
                fetched_at: UNIX_EPOCH + Duration::from_secs(row.get::<_, i64>(2)?.max(0) as u64),
                last_used: UNIX_EPOCH + Duration::from_secs(row.get::<_, i64>(3)?.max(0) as u64),
                size: row.get::<_, i64>(4)?.max(0) as u64,
                max_age: Duration::from_secs(row.get::<_, i64>(5)?.max(0) as u64),
            })
        })
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_cache(cap: u64, ttl_secs: u64) -> (tempfile::TempDir, Cache) {
        let d = tempdir().unwrap();
        let c = Cache::open(d.path(), cap, Duration::from_secs(ttl_secs)).unwrap();
        (d, c)
    }

    #[test]
    fn insert_then_lookup_roundtrip() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], 300);
        let hit = c.lookup("https://a").unwrap();
        assert_eq!(hit.etag, "etag1");
        assert_eq!(hit.brotli_body, vec![1, 2, 3]);
        assert_eq!(hit.max_age, Duration::from_secs(300));
    }

    #[test]
    fn negative_max_age_skips_write() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], -1);
        assert!(c.lookup("https://a").is_none());
    }

    #[test]
    fn zero_max_age_is_stored_but_never_fresh() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], 0);
        let hit = c.lookup("https://a").unwrap();
        assert!(!hit.is_fresh(SystemTime::now()));
    }

    #[test]
    fn max_ttl_caps_server_max_age() {
        let (_d, c) = open_cache(1_000_000, 60);
        c.insert("https://a", "etag1", &[1, 2, 3], 999_999);
        let hit = c.lookup("https://a").unwrap();
        assert_eq!(hit.max_age, Duration::from_secs(60));
    }

    #[test]
    fn is_fresh_boundaries() {
        let hit = Hit {
            etag: "e".to_string(),
            brotli_body: vec![],
            fetched_at: SystemTime::now(),
            max_age: Duration::from_secs(1),
        };
        assert!(hit.is_fresh(SystemTime::now()));
        assert!(!hit.is_fresh(SystemTime::now() + Duration::from_secs(2)));
    }

    #[test]
    fn touch_last_used_updates_ordering() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://old", "e", &[0u8; 10], 300);
        std::thread::sleep(Duration::from_millis(1100)); // seconds-resolution timestamp
        c.insert("https://new", "e", &[0u8; 10], 300);
        std::thread::sleep(Duration::from_millis(1100));
        c.touch_last_used("https://old");
        let entries = c.list();
        assert_eq!(entries[0].url, "https://old");
    }

    #[test]
    fn lru_evicts_least_recently_used_on_overflow() {
        // Cap = 30 bytes; insert three 20-byte entries: overflow after 2nd, evict oldest.
        let (_d, c) = open_cache(30, 600);
        c.insert("https://a", "e", &[0u8; 20], 300);
        std::thread::sleep(Duration::from_millis(1100));
        c.insert("https://b", "e", &[0u8; 20], 300);
        assert!(c.lookup("https://a").is_none(), "oldest should have been evicted");
        assert!(c.lookup("https://b").is_some());
    }

    #[test]
    fn delete_and_clear() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "e", &[1], 300);
        c.insert("https://b", "e", &[1], 300);
        c.delete("https://a");
        assert!(c.lookup("https://a").is_none());
        assert!(c.lookup("https://b").is_some());
        c.clear();
        assert!(c.lookup("https://b").is_none());
    }

    #[test]
    fn touch_fresh_bumps_fetched_at() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "e", &[1], 300);
        let before = c.lookup("https://a").unwrap().fetched_at;
        std::thread::sleep(Duration::from_millis(1100));
        c.touch_fresh("https://a");
        let after = c.lookup("https://a").unwrap().fetched_at;
        assert!(after > before);
    }
}

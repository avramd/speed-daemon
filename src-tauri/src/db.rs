use crate::model::Bucket;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const WEEK_MS: u64 = 7 * 24 * 3600 * 1000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// On-disk history. `samples(target_id, ts, rtt)` with `rtt` NULL = a probe that ran but
/// got no reply (loss). `uptime(start, last)` records each run of the daemon; a heartbeat
/// advances `last`, so gaps between uptime rows are time the app wasn't running.
pub struct Db {
    conn: Mutex<Connection>,
    /// rowid of this run's uptime row.
    session: i64,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS samples (target_id TEXT NOT NULL, ts INTEGER NOT NULL, rtt REAL);
             CREATE INDEX IF NOT EXISTS idx_samples ON samples(target_id, ts);
             CREATE TABLE IF NOT EXISTS uptime (start INTEGER NOT NULL, last INTEGER NOT NULL);",
        )?;
        let now = now_ms() as i64;
        conn.execute("INSERT INTO uptime (start, last) VALUES (?1, ?1)", [now])?;
        let session = conn.last_insert_rowid();
        Ok(Db {
            conn: Mutex::new(conn),
            session,
        })
    }

    pub fn insert(&self, target_id: &str, ts: u64, rtt: Option<f64>) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO samples (target_id, ts, rtt) VALUES (?1, ?2, ?3)",
            rusqlite::params![target_id, ts as i64, rtt],
        );
    }

    /// Advance this session's `last` so downtime can be inferred after a restart.
    pub fn heartbeat(&self) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE uptime SET last = ?1 WHERE rowid = ?2",
            rusqlite::params![now_ms() as i64, self.session],
        );
    }

    /// Drop anything older than a week (called on startup and periodically).
    pub fn prune(&self) {
        let cutoff = now_ms().saturating_sub(WEEK_MS) as i64;
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM samples WHERE ts < ?1", [cutoff]);
        let _ = conn.execute("DELETE FROM uptime WHERE last < ?1", [cutoff]);
    }

    /// Earliest known instant (oldest sample or first uptime start) for slider bounds.
    pub fn oldest(&self) -> Option<u64> {
        let conn = self.conn.lock().unwrap();
        let s: Option<i64> = conn
            .query_row("SELECT MIN(ts) FROM samples", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten();
        let u: Option<i64> = conn
            .query_row("SELECT MIN(start) FROM uptime", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten();
        match (s, u) {
            (Some(a), Some(b)) => Some(a.min(b) as u64),
            (Some(a), None) => Some(a as u64),
            (None, Some(b)) => Some(b as u64),
            (None, None) => None,
        }
    }

    /// Aggregate [from, to) into `buckets` equal time slices for one target.
    pub fn window(&self, target_id: &str, from: u64, to: u64, buckets: usize) -> Vec<Bucket> {
        let n = buckets.max(1);
        let span = to.saturating_sub(from).max(1);
        let conn = self.conn.lock().unwrap();

        let mut out: Vec<Bucket> = (0..n)
            .map(|i| Bucket {
                t: from + span * i as u64 / n as u64,
                worst: None,
                loss: 0.0,
                count: 0,
                up: false,
            })
            .collect();

        // Bucket samples: worst (max) RTT, loss fraction, count.
        if let Ok(mut stmt) = conn.prepare(
            "SELECT ((ts - ?1) * ?2 / ?3) AS b, \
                    MAX(rtt), \
                    SUM(CASE WHEN rtt IS NULL THEN 1 ELSE 0 END), \
                    COUNT(*) \
             FROM samples \
             WHERE target_id = ?4 AND ts >= ?1 AND ts < ?5 \
             GROUP BY b",
        ) {
            let rows = stmt.query_map(
                rusqlite::params![from as i64, n as i64, span as i64, target_id, to as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            );
            if let Ok(rows) = rows {
                for (b, worst, lost, cnt) in rows.flatten() {
                    let i = (b.max(0) as usize).min(n - 1);
                    out[i].worst = worst;
                    out[i].count = cnt as u32;
                    out[i].loss = if cnt > 0 { lost as f64 / cnt as f64 } else { 0.0 };
                }
            }
        }

        // Mark buckets overlapping an uptime interval (everything else renders grey).
        if let Ok(mut stmt) =
            conn.prepare("SELECT start, last FROM uptime WHERE last >= ?1 AND start < ?2")
        {
            let intervals: Vec<(u64, u64)> = stmt
                .query_map(rusqlite::params![from as i64, to as i64], |r| {
                    Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
                })
                .map(|it| it.flatten().collect())
                .unwrap_or_default();
            for (start, last) in intervals {
                let lo = start.max(from).saturating_sub(from) * n as u64 / span;
                let hi = last.min(to.saturating_sub(1)).saturating_sub(from) * n as u64 / span;
                let lo = (lo as usize).min(n - 1);
                let hi = (hi as usize).min(n - 1);
                for b in out.iter_mut().take(hi + 1).skip(lo) {
                    b.up = true;
                }
            }
        }

        out
    }
}

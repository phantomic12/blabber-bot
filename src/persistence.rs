//! SQLite-backed persistence for per-guild queues and settings.
//!
//! State is saved as JSON blobs — simple and schema-stable — and restored at
//! startup. Persisted tracks keep their page URLs but not their stream URLs,
//! so restored tracks re-resolve fresh ones when they start playing.

use crate::extractor::TrackMeta;
use crate::player::LoopMode;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, RoleId};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Everything worth keeping for a guild between restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedGuild {
    pub current: Option<TrackMeta>,
    pub queue: Vec<TrackMeta>,
    pub loop_mode: LoopMode,
    pub volume: f32,
    pub autoplay: bool,
    pub dj_role: Option<RoleId>,
    pub requester_only: bool,
    pub vote_skip: bool,
    pub vote_skip_ratio: f32,
    pub text_channel: Option<ChannelId>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Opens (creating if needed) the state database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Arc<Self>> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS guild_state (
                guild_id TEXT PRIMARY KEY,
                state_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )?;

        Ok(Arc::new(Self { conn: Mutex::new(conn) }))
    }

    /// Loads every persisted guild state.
    pub async fn load_all(&self) -> anyhow::Result<Vec<(u64, PersistedGuild)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT guild_id, state_json FROM guild_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (guild_id, json) = row?;
            let parsed = guild_id.parse::<u64>().ok().zip(serde_json::from_str(&json).ok());
            if let Some((guild_id, state)) = parsed {
                out.push((guild_id, state));
            }
        }
        Ok(out)
    }

    /// Upserts a guild's state.
    pub async fn save(&self, guild_id: u64, state: &PersistedGuild) -> anyhow::Result<()> {
        let json = serde_json::to_string(state)?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO guild_state (guild_id, state_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id) DO UPDATE SET state_json = ?2, updated_at = ?3",
            rusqlite::params![guild_id.to_string(), json, now_unix()],
        )?;
        Ok(())
    }

    /// Drops a guild's state entirely.
    pub async fn delete(&self, guild_id: u64) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM guild_state WHERE guild_id = ?1",
            rusqlite::params![guild_id.to_string()],
        )?;
        Ok(())
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

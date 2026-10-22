//! Runtime configuration, loaded from environment variables (and a local `.env`).

use anyhow::Context as _;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Discord bot token (`DISCORD_TOKEN`).
    pub token: String,
    /// Path to the `yt-dlp` executable (`YTDLP_PATH`, default `yt-dlp`).
    pub ytdlp_path: String,
    /// Netscape-format cookie file forwarded to yt-dlp (`YTDLP_COOKIES`).
    pub cookies_file: Option<PathBuf>,
    /// Browser name to read cookies from, e.g. `firefox` (`YTDLP_COOKIES_FROM_BROWSER`).
    pub cookies_from_browser: Option<String>,
    /// Extra arguments appended to every yt-dlp invocation (`YTDLP_EXTRA_ARGS`).
    pub ytdlp_extra_args: Vec<String>,
    /// Maximum number of playlist entries accepted in one request (`MAX_PLAYLIST`).
    pub max_playlist: usize,
    /// Default playback volume in percent (`DEFAULT_VOLUME`).
    pub default_volume: u32,
    /// Whether new guilds start with autoplay enabled (`AUTOPLAY`, default on).
    pub autoplay: bool,
    /// When set, slash commands are registered on this guild only — visible
    /// instantly, which is ideal while testing (`DISCORD_GUILD_ID`).
    pub guild_id: Option<u64>,
    /// Where the SQLite state database lives (`DATABASE_PATH`).
    pub database_path: PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        // A local `.env` is convenient but entirely optional.
        let _ = dotenvy::dotenv();

        let token = std::env::var("DISCORD_TOKEN")
            .context("DISCORD_TOKEN is not set (export it or put it in a .env file)")?;

        let cookies_file = opt_var("YTDLP_COOKIES").map(|v| expand_home(v.as_str()));
        if let Some(path) = &cookies_file {
            if !path.is_file() {
                anyhow::bail!(
                    "YTDLP_COOKIES points to '{}', but no such file exists",
                    path.display()
                );
            }
        }

        let max_playlist = opt_var("MAX_PLAYLIST")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(25)
            .clamp(1, 100);

        let default_volume = opt_var("DEFAULT_VOLUME")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(100)
            .clamp(0, 200);

        let autoplay = opt_var("AUTOPLAY")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);

        Ok(Self {
            token,
            ytdlp_path: opt_var("YTDLP_PATH").unwrap_or_else(|| "yt-dlp".to_string()),
            cookies_file,
            cookies_from_browser: opt_var("YTDLP_COOKIES_FROM_BROWSER"),
            ytdlp_extra_args: opt_var("YTDLP_EXTRA_ARGS")
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            max_playlist,
            default_volume,
            autoplay,
            guild_id: opt_var("DISCORD_GUILD_ID").and_then(|v| v.parse::<u64>().ok()),
            database_path: opt_var("DATABASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("blabber-bot.db")),
        })
    }

    /// Default volume expressed as the multiplier songbird expects.
    pub fn default_volume_scale(&self) -> f32 {
        self.default_volume as f32 / 100.0
    }
}

fn opt_var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Unix-friendly path handling: expand a leading `~/` to the user's home
/// directory (`YTDLP_COOKIES=~/cookies.txt`). Everything else — relative and
/// absolute paths — is passed through untouched.
fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_prefix() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~/cookies.txt"), home.join("cookies.txt"));
            assert_eq!(expand_home("~"), home);
        }
    }

    #[test]
    fn leaves_other_paths_alone() {
        assert_eq!(expand_home("./cookies.txt"), PathBuf::from("./cookies.txt"));
        assert_eq!(expand_home("cookies.txt"), PathBuf::from("cookies.txt"));
        assert_eq!(expand_home("/home/bot/cookies.txt"), PathBuf::from("/home/bot/cookies.txt"));
    }
}

//! Runtime configuration, loaded from environment variables (and a local `.env`).

use anyhow::Context as _;
use std::path::PathBuf;

/// Default grace period before leaving an empty voice channel, in seconds.
const DEFAULT_EMPTY_TIMEOUT_SECS: u64 = 300;

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
    /// How long to keep playing in a voice channel with nobody else in it
    /// (`EMPTY_CHANNEL_TIMEOUT`, seconds; `0` disables the behaviour).
    pub empty_channel_timeout: Option<std::time::Duration>,
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

        // Discord voice is a shared experience; a bot playing to an empty
        // channel just burns bandwidth and yt-dlp lookups.
        let empty_channel_timeout = parse_empty_timeout(opt_var("EMPTY_CHANNEL_TIMEOUT").as_deref());

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
            empty_channel_timeout,
        })
    }

    /// Default volume expressed as the multiplier songbird expects.
    pub fn default_volume_scale(&self) -> f32 {
        self.default_volume as f32 / 100.0
    }
}

/// Parses `EMPTY_CHANNEL_TIMEOUT` into the idle grace period.
///
/// Unset or unparseable falls back to the 5-minute default; an explicit `0`
/// (and only `0`) disables the behaviour entirely. Silence about the setting
/// should give sensible behaviour rather than an unexpected opt-out, so a typo
/// must not be read as "never time out".
fn parse_empty_timeout(raw: Option<&str>) -> Option<std::time::Duration> {
    match raw {
        None => Some(std::time::Duration::from_secs(DEFAULT_EMPTY_TIMEOUT_SECS)),
        Some(value) => match value.parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(std::time::Duration::from_secs(secs)),
            Err(_) => Some(std::time::Duration::from_secs(DEFAULT_EMPTY_TIMEOUT_SECS)),
        },
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
    fn empty_timeout_defaults_to_five_minutes() {
        assert_eq!(
            parse_empty_timeout(None),
            Some(std::time::Duration::from_secs(300))
        );
    }

    #[test]
    fn explicit_zero_disables_the_idle_timeout() {
        assert_eq!(parse_empty_timeout(Some("0")), None);
    }

    #[test]
    fn empty_timeout_honours_a_custom_value() {
        assert_eq!(
            parse_empty_timeout(Some("90")),
            Some(std::time::Duration::from_secs(90))
        );
    }

    #[test]
    fn garbage_empty_timeout_falls_back_to_the_default() {
        // A typo must not silently mean "never disconnect".
        assert_eq!(
            parse_empty_timeout(Some("five minutes")),
            Some(std::time::Duration::from_secs(300))
        );
    }

    #[test]
    fn leaves_other_paths_alone() {
        assert_eq!(expand_home("./cookies.txt"), PathBuf::from("./cookies.txt"));
        assert_eq!(expand_home("cookies.txt"), PathBuf::from("cookies.txt"));
        assert_eq!(expand_home("/home/bot/cookies.txt"), PathBuf::from("/home/bot/cookies.txt"));
    }
}

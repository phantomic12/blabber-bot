//! Media resolution via `yt-dlp`, with age-restricted video support.
//!
//! Age-restricted YouTube videos require an account session to extract. The
//! extractor therefore runs a chain of strategies: the configured cookie
//! session first, then alternative player clients (`tv`/`android`), which
//! occasionally bypass the age gate outright. Users can also push arbitrary
//! yt-dlp flags through `YTDLP_EXTRA_ARGS`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// How long a resolved stream URL is considered fresh before being re-resolved.
pub const STREAM_TTL: Duration = Duration::from_secs(60 * 60);

/// Number of related tracks autoplay queues per fill.
const AUTOPLAY_BATCH: usize = 5;
/// Results pulled when searching for related tracks.
const RELATED_SEARCH: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMeta {
    pub title: String,
    /// Public page URL; used to re-resolve a stream URL when the old one expires.
    pub page_url: String,
    pub thumbnail: Option<String>,
    pub uploader: Option<String>,
    /// Duration in seconds; `None` for live streams or unknown lengths.
    pub duration: Option<f64>,
    pub is_live: bool,
    pub age_limit: Option<u64>,
    /// Direct media URL; `None` for lazily resolved playlist entries.
    pub stream_url: Option<String>,
    pub http_headers: HashMap<String, String>,
    pub protocol: Option<String>,
    pub filesize: Option<u64>,
    /// When `stream_url` was last resolved. Skipped in persistence: restored
    /// tracks re-resolve fresh stream URLs when they start playing.
    #[serde(skip)]
    pub resolved_at: Option<Instant>,
    /// Display name of the user who requested this track.
    pub requester: String,
    /// Snowflake of the requesting user, for requester-only enforcement.
    #[serde(default)]
    pub requester_id: Option<u64>,
}

impl TrackMeta {
    pub fn age_restricted(&self) -> bool {
        self.age_limit.is_some_and(|age| age >= 18)
    }

    /// Whether the stream URL is missing or old enough that it should be
    /// resolved again before use.
    pub fn needs_resolution(&self) -> bool {
        match (self.stream_url.as_ref(), self.resolved_at) {
            (Some(_), Some(at)) => at.elapsed() > STREAM_TTL,
            _ => true,
        }
    }
}

#[derive(Debug)]
pub enum ExtractError {
    /// The `yt-dlp` binary could not be found or spawned.
    MissingBinary(String),
    /// The video exists but demands an age-verified session.
    AgeRestricted(String),
    /// The video is private or members-only.
    Private(String),
    /// Nothing playable was found for the query.
    NotFound(String),
    /// `yt-dlp` timed out.
    Timeout,
    /// Anything else `yt-dlp` complained about.
    Failed(String),
}

impl ExtractError {
    /// Human-readable hint shown to the user alongside the error.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::AgeRestricted(_) | Self::Private(_) => {
                "Age-restricted and private videos need a logged-in YouTube session: export a \
                 cookies.txt from a browser signed into an age-verified account and set \
                 YTDLP_COOKIES=/path/to/cookies.txt (or YTDLP_COOKIES_FROM_BROWSER=firefox, \
                 chrome, ...). See the README for details."
            },
            Self::MissingBinary(_) => {
                "Install yt-dlp and make sure it is on PATH (or point YTDLP_PATH at it). \
                 YouTube extraction also needs a JS runtime such as Deno or Node.js 20+."
            },
            _ => "",
        }
    }

    fn classify(stderr: &str) -> Self {
        let text = stderr.to_lowercase();
        if text.contains("sign in to confirm your age")
            || text.contains("age-restricted")
            || text.contains("age restricted")
            || text.contains("inappropriate for some users")
        {
            Self::AgeRestricted(stderr.to_string())
        } else if text.contains("private video")
            || text.contains("sign in if you")
            || text.contains("members-only")
            || text.contains("join this channel")
        {
            Self::Private(stderr.to_string())
        } else if text.contains("no results")
            || text.contains("video unavailable")
            || text.contains("not available")
            || text.contains("does not exist")
        {
            Self::NotFound(stderr.to_string())
        } else {
            Self::Failed(stderr.to_string())
        }
    }
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBinary(program) => write!(f, "could not run '{program}'"),
            Self::AgeRestricted(raw) => {
                write!(f, "{}", headline_or(raw, "this video is age-restricted"))
            },
            Self::Private(raw) => {
                write!(f, "{}", headline_or(raw, "this video is private or members-only"))
            },
            Self::NotFound(raw) => {
                write!(f, "{}", headline_or(raw, "no playable results found"))
            },
            Self::Timeout => write!(f, "yt-dlp timed out"),
            Self::Failed(raw) => {
                write!(f, "yt-dlp failed: {}", headline_or(raw, "unknown error"))
            },
        }
    }
}

/// First meaningful line of a yt-dlp error report, without its `ERROR:` tag.
fn headline(raw: &str) -> &str {
    let line = raw.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    line.strip_prefix("ERROR: ").unwrap_or(line)
}

fn headline_or(raw: &str, fallback: &str) -> String {
    let line = headline(raw);
    if line.is_empty() {
        fallback.to_string()
    } else {
        line.to_string()
    }
}

impl std::error::Error for ExtractError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extraction {
    /// Full extraction: resolves a playable stream URL and complete metadata.
    Full,
    /// Flat extraction: fast playlist listing; stream URLs resolve lazily.
    Flat,
}

pub struct Extractor {
    /// yt-dlp invocation: program plus any fixed arguments (so wrappers like
    /// `py -3 -m yt_dlp` work via `YTDLP_PATH`).
    program: Vec<String>,
    cookies_file: Option<String>,
    cookies_from_browser: Option<String>,
    extra_args: Vec<String>,
    max_playlist: usize,
    format: String,
}

impl Extractor {
    pub fn new(config: &crate::config::Config) -> Self {
        let mut program: Vec<String> = config
            .ytdlp_path
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if program.is_empty() {
            program.push("yt-dlp".to_string());
        }

        Self {
            program,
            cookies_file: config.cookies_file.as_ref().map(|p| p.display().to_string()),
            cookies_from_browser: config.cookies_from_browser.clone(),
            extra_args: config.ytdlp_extra_args.clone(),
            max_playlist: config.max_playlist,
            // Prefer WebM/Opus, which songbird can pass through to Discord
            // without re-encoding.
            format: "bestaudio[ext=webm]/bestaudio/best".to_string(),
        }
    }

    /// Resolves a user query (search text or URL) into playable tracks.
    pub async fn resolve(&self, query: &str, requester: &str) -> Result<Vec<TrackMeta>, ExtractError> {
        self.resolve_detailed(query, requester).await.map(|(tracks, _)| tracks)
    }

    /// As [`resolve`], but also reports which strategy unlocked the content —
    /// used by the `/diagnose` preflight.
    pub async fn resolve_detailed(
        &self,
        query: &str,
        requester: &str,
    ) -> Result<(Vec<TrackMeta>, &'static str), ExtractError> {
        let target = build_target(query);
        let mode = if is_playlist_target(&target) {
            Extraction::Flat
        } else {
            Extraction::Full
        };
        self.extract(&target, mode, requester).await
    }

    /// Runs `yt-dlp --version`, returning the reported version string.
    pub async fn version(&self) -> Result<String, ExtractError> {
        use tokio::process::Command;

        let output = Command::new(&self.program[0])
            .args(&self.program[1..])
            .arg("--version")
            .output()
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ExtractError::MissingBinary(self.program_label())
                } else {
                    ExtractError::Failed(err.to_string())
                }
            })?;

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Human-readable description of the configured cookie session.
    pub fn cookie_status(&self) -> String {
        if let Some(path) = &self.cookies_file {
            let cookies = std::fs::read_to_string(path).map(|text| {
                text.lines()
                    .filter(|line| !line.starts_with('#') && line.contains("youtube.com"))
                    .count()
            });
            match cookies {
                Ok(count) => format!("Cookie session: `{path}` ({count} youtube.com cookies)"),
                Err(err) => format!("Cookie session: ❌ can't read `{path}`: {err}"),
            }
        } else if let Some(browser) = &self.cookies_from_browser {
            format!("Cookie session: `--cookies-from-browser {browser}`")
        } else {
            "Cookie session: _none configured — age-restricted videos will need YTDLP_COOKIES_"
                .to_string()
        }
    }

    fn program_label(&self) -> String {
        self.program.join(" ")
    }

    /// Resolves a single page URL into one playable track with a fresh stream URL.
    pub async fn resolve_one(&self, page_url: &str, requester: &str) -> Result<TrackMeta, ExtractError> {
        let (tracks, _) = self.extract(page_url, Extraction::Full, requester).await?;
        tracks
            .into_iter()
            .next()
            .ok_or_else(|| ExtractError::NotFound(format!("no playable media at {page_url}")))
    }

    /// Finds tracks related to `seed`, for autoplay when the queue runs dry.
    ///
    /// Best-effort: tries the seed's YouTube mix playlist first (genuinely
    /// related videos), falling back to searching for more from the same
    /// uploader. Tracks in `exclude` (recently played) are filtered out.
    pub async fn related(
        &self,
        seed: &TrackMeta,
        exclude: &[String],
        requester: &str,
    ) -> Vec<TrackMeta> {
        let mut candidates = Vec::new();

        if let Some(id) = youtube_video_id(&seed.page_url) {
            // The `RD` mix playlist is YouTube's radio seeded by a video.
            let mix = format!("https://www.youtube.com/watch?v={id}&list=RD{id}");
            candidates = self.resolve(&mix, requester).await.unwrap_or_default();
        }

        if candidates.len() < 2 {
            let query = match &seed.uploader {
                Some(uploader) => format!("ytsearch{RELATED_SEARCH}:{uploader} {}", seed.title),
                None => format!("ytsearch{RELATED_SEARCH}:{}", seed.title),
            };
            candidates = self.resolve(&query, requester).await.unwrap_or_default();
        }

        candidates
            .into_iter()
            .filter(|track| track.page_url != seed.page_url)
            .filter(|track| !exclude.contains(&track.page_url))
            .take(AUTOPLAY_BATCH)
            .collect()
    }

    async fn extract(
        &self,
        target: &str,
        mode: Extraction,
        requester: &str,
    ) -> Result<(Vec<TrackMeta>, &'static str), ExtractError> {
        let mut last_err: Option<ExtractError> = None;

        for (label, strategy) in self.strategies() {
            match self.run(target, mode, requester, &strategy).await {
                Ok(tracks) if !tracks.is_empty() => return Ok((tracks, label)),
                // No results: retrying with other credentials won't help.
                Ok(_) => return Err(ExtractError::NotFound(format!("no results for '{target}'"))),
                Err(err) => {
                    tracing::debug!(target, strategy = label, error = %err, "extraction strategy failed");
                    last_err = Some(err);
                },
            }
        }

        Err(last_err.unwrap_or_else(|| ExtractError::NotFound(target.to_string())))
    }

    /// Argument sets to try in order. The first entry carries the configured
    /// cookie session if one exists — that is what unlocks age-restricted,
    /// private and members-only videos. Later strategies fall back to player
    /// clients which occasionally bypass the age gate without a session.
    fn strategies(&self) -> Vec<(&'static str, Vec<String>)> {
        let cookie_args = self.cookie_args();
        let age_gate_bypass =
            vec!["--extractor-args".to_string(), "youtube:player_client=tv,android".to_string()];

        let mut strategies = Vec::new();
        if !cookie_args.is_empty() {
            strategies.push(("cookie session", cookie_args.clone()));
            strategies.push((
                "cookie session + tv/android player",
                [cookie_args.clone(), age_gate_bypass.clone()].concat(),
            ));
            strategies.push(("tv/android player (age-gate bypass)", age_gate_bypass));
            strategies.push(("default player", Vec::new()));
        } else {
            strategies.push(("tv/android player (age-gate bypass)", age_gate_bypass));
            strategies.push(("default player", Vec::new()));
        }
        strategies
    }

    fn cookie_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(path) = &self.cookies_file {
            args.push("--cookies".to_string());
            args.push(path.clone());
        }
        if let Some(browser) = &self.cookies_from_browser {
            args.push("--cookies-from-browser".to_string());
            args.push(browser.clone());
        }
        args
    }

    async fn run(
        &self,
        target: &str,
        mode: Extraction,
        requester: &str,
        strategy: &[String],
    ) -> Result<Vec<TrackMeta>, ExtractError> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::new(&self.program[0]);
        cmd.args(&self.program[1..]);
        cmd.arg("--dump-json")
            .arg("--no-warnings")
            .arg("--no-progress")
            .arg("--ignore-errors")
            .arg("--socket-timeout")
            .arg("20")
            .arg("-f")
            .arg(&self.format)
            .arg("--playlist-end")
            .arg(self.max_playlist.to_string());

        if mode == Extraction::Flat {
            cmd.arg("--flat-playlist");
        }

        // Playlist-ish targets expand to their entries (`watch?v=..&list=..`
        // links point at both a video and a playlist); everything else
        // resolves to just the video it points at.
        if is_playlist_target(target) {
            cmd.arg("--yes-playlist");
        } else {
            cmd.arg("--no-playlist");
        }

        cmd.args(&self.extra_args)
            .args(strategy)
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| ExtractError::Timeout)?
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    ExtractError::MissingBinary(self.program_label())
                } else {
                    ExtractError::Failed(err.to_string())
                }
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let tracks: Vec<TrackMeta> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str::<YtDlpInfo>(line) {
                Ok(info) => info.into_meta(mode, requester),
                Err(err) => {
                    tracing::debug!(%err, "skipping unparseable yt-dlp entry");
                    None
                },
            })
            .collect();

        // `--ignore-errors` keeps going past broken playlist entries but still
        // exits non-zero, so parsed entries take precedence over exit status.
        if !tracks.is_empty() {
            Ok(tracks)
        } else if output.status.success() || !stderr.trim().is_empty() {
            Err(ExtractError::classify(&stderr))
        } else {
            Err(ExtractError::Failed(format!(
                "yt-dlp exited with {} and produced no output",
                output.status
            )))
        }
    }
}

/// A subset of the yt-dlp info dict we care about.
#[derive(Debug, Deserialize)]
struct YtDlpInfo {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    filesize: Option<u64>,
    #[serde(default)]
    http_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    age_limit: Option<u64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

impl YtDlpInfo {
    fn into_meta(self, mode: Extraction, requester: &str) -> Option<TrackMeta> {
        // In flat mode `url` is the page URL; in full mode it is the stream URL.
        let (stream_url, page_url) = match mode {
            Extraction::Full => {
                let page = self.webpage_url.clone().or_else(|| self.url.clone())?;
                (self.url.clone(), page)
            },
            Extraction::Flat => {
                let page = self.webpage_url.clone().or_else(|| self.url.clone())?;
                (None, page)
            },
        };

        if mode == Extraction::Full && stream_url.is_none() {
            return None;
        }

        Some(TrackMeta {
            title: self
                .title
                .or(self.track)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "Unknown title".to_string()),
            page_url,
            thumbnail: self.thumbnail,
            uploader: self.uploader.or(self.channel),
            duration: self.duration,
            is_live: self.is_live.unwrap_or(false),
            age_limit: self.age_limit,
            stream_url,
            http_headers: self.http_headers.unwrap_or_default(),
            protocol: self.protocol,
            filesize: self.filesize,
            resolved_at: (mode == Extraction::Full).then(Instant::now),
            requester: requester.to_string(),
            requester_id: None,
        })
    }
}

/// Turns a user-supplied query into a yt-dlp target: URLs pass through
/// untouched, anything else becomes a YouTube search.
fn build_target(query: &str) -> String {
    let query = query.trim();
    let lower = query.to_lowercase();
    if query.contains("://") || lower.starts_with("ytsearch") || lower.starts_with("scsearch") {
        query.to_string()
    } else {
        format!("ytsearch1:{query}")
    }
}

/// Detects a JavaScript runtime usable by yt-dlp for YouTube extraction.
/// Returns a short description of the first one found.
pub async fn js_runtime() -> Option<String> {
    for program in ["deno", "node", "bun"] {
        if let Ok(output) = tokio::process::Command::new(program).arg("--version").output().await {
            if output.status.success() {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return Some(format!("{program} `{version}`"));
            }
        }
    }
    None
}

/// Whether the target is a playlist that should be listed lazily rather than
/// fully extracted up front.
fn is_playlist_target(target: &str) -> bool {
    let lower = target.to_lowercase();
    lower.contains("youtube.com/playlist")
        // `watch?v=..&list=..` style links: playlists and YouTube mixes.
        || lower.contains("list=")
        || (lower.contains("soundcloud.com/") && lower.contains("/sets/"))
        || lower.contains("soundcloud.com/profiles/")
        || (lower.starts_with("ytsearch") && !lower.starts_with("ytsearch1:"))
}

/// Extracts an 11-character YouTube video ID from common URL shapes
/// (`watch?v=`, `youtu.be/`, `/shorts/`, `/embed/`, `/live/`).
fn youtube_video_id(url: &str) -> Option<String> {
    let after = |marker: &str| url.split_once(marker).map(|(_, rest)| rest);

    let raw = after("youtu.be/")
        .or_else(|| after("v="))
        .or_else(|| after("/shorts/"))
        .or_else(|| after("/embed/"))
        .or_else(|| after("/live/"))?;

    let id: String = raw
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();

    (id.len() == 11).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_becomes_search() {
        assert_eq!(build_target("never gonna give you up"), "ytsearch1:never gonna give you up");
    }

    #[test]
    fn urls_pass_through() {
        assert_eq!(
            build_target("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[test]
    fn playlists_are_detected() {
        assert!(is_playlist_target("https://www.youtube.com/playlist?list=PL123"));
        assert!(is_playlist_target("https://soundcloud.com/user/sets/mix"));
        assert!(is_playlist_target("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ"));
        assert!(!is_playlist_target("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
    }

    #[test]
    fn extracts_youtube_video_ids() {
        assert_eq!(
            youtube_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_video_id("https://youtu.be/dQw4w9WgXcQ?t=42").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_video_id("https://www.youtube.com/shorts/abcDEF12345").as_deref(),
            Some("abcDEF12345")
        );
        assert_eq!(youtube_video_id("https://soundcloud.com/user/track"), None);
    }

    #[test]
    fn age_gate_errors_are_classified() {
        assert!(matches!(
            ExtractError::classify("ERROR: Sign in to confirm your age"),
            ExtractError::AgeRestricted(_)
        ));
    }
}

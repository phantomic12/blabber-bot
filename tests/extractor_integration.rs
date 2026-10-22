//! Integration tests for the extractor, driven by a fake `yt-dlp` binary.
//!
//! The fake is compiled with `rustc` at test time and simulates age gates,
//! failures, playlists, and flaky extraction — so the strategy fallback chain
//! is testable without network access.

use blabber_bot::config::Config;
use blabber_bot::extractor::{ExtractError, Extractor};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Emulates the slice of yt-dlp behaviour the extractor relies on. Wrapper
/// options (`--log <path>`, `--fail-first`) arrive before the yt-dlp args.
const FAKE_YTDLP_SOURCE: &str = r##"
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut log_path: Option<String> = None;
    let mut fail_first = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--log" => {
                log_path = args.get(index + 1).cloned();
                index += 2;
            },
            "--fail-first" => {
                fail_first = true;
                index += 1;
            },
            _ => break,
        }
    }
    let ytdlp_args = &args[index..];

    let mut invocations = 0;
    if let Some(path) = &log_path {
        invocations = append_line(path, &ytdlp_args.join(" "));
    }

    if ytdlp_args.iter().any(|arg| arg == "--version") {
        println!("2026.01.01-fake");
        return;
    }

    let target = ytdlp_args.last().cloned().unwrap_or_default();
    let has_cookies = ytdlp_args.iter().any(|arg| arg == "--cookies");
    let yes_playlist = ytdlp_args.iter().any(|arg| arg == "--yes-playlist");

    if fail_first && invocations == 1 {
        eprintln!("ERROR: temporary failure");
        std::process::exit(1);
    }
    if target.contains("age-gated") && !has_cookies {
        eprintln!("ERROR: Sign in to confirm your age");
        std::process::exit(1);
    }
    if target.contains("unavailable") {
        eprintln!("ERROR: Video unavailable");
        std::process::exit(1);
    }

    if target.contains("playlist") && yes_playlist {
        for entry in 0..3 {
            print_entry(entry);
        }
    } else {
        print_entry(0);
    }
}

fn append_line(path: &str, line: &str) -> u32 {
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
    std::fs::read_to_string(path).map(|text| text.lines().count() as u32).unwrap_or(0)
}

fn print_entry(index: usize) {
    let json = r#"{"title": "Fake Track @I@", "webpage_url": "https://example.com/@I@", "url": "https://example.com/stream/@I@.webm", "duration": 123.0, "uploader": "Fake Artist", "age_limit": 18, "is_live": false, "protocol": "https", "http_headers": {"user-agent": "fake"}}"#;
    println!("{}", json.replace("@I@", &index.to_string()));
}
"##;

fn fake_binary() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir = std::env::temp_dir().join("blabber-bot-fake-ytdlp");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let src = dir.join("fake-ytdlp.rs");
        std::fs::write(&src, FAKE_YTDLP_SOURCE).expect("write fake source");
        let exe = dir.join(format!("fake-ytdlp{}", std::env::consts::EXE_SUFFIX));

        let status = std::process::Command::new("rustc")
            .args(["-O", "-o"])
            .arg(&exe)
            .arg(&src)
            .status()
            .expect("rustc is required to build the fake yt-dlp");
        assert!(status.success(), "failed to compile the fake yt-dlp");
        exe
    })
}

/// Builds an extractor around the fake, with its own invocation log.
fn fake_extractor(tag: &str, cookies: Option<PathBuf>, fail_first: bool) -> (Extractor, PathBuf) {
    let dir = std::env::temp_dir().join("blabber-bot-fake-ytdlp");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log = dir.join(format!("{tag}.log"));
    let _ = std::fs::remove_file(&log);

    let mut program = fake_binary().display().to_string();
    program.push_str(&format!(" --log {}", log.display()));
    if fail_first {
        program.push_str(" --fail-first");
    }

    let config = Config {
        token: String::new(),
        ytdlp_path: program,
        cookies_file: cookies,
        cookies_from_browser: None,
        ytdlp_extra_args: Vec::new(),
        max_playlist: 25,
        default_volume: 100,
        autoplay: true,
        guild_id: None,
        database_path: PathBuf::from(":memory:"),
    };

    (Extractor::new(&config), log)
}

fn log_lines(path: &Path) -> usize {
    std::fs::read_to_string(path).map(|text| text.lines().count()).unwrap_or(0)
}

#[tokio::test]
async fn resolves_a_search_query() {
    let (extractor, _) = fake_extractor("search", None, false);

    let tracks = extractor.resolve("some song", "tester").await.expect("resolve");
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Fake Track 0");
    assert_eq!(tracks[0].requester, "tester");
    assert!(tracks[0].stream_url.is_some(), "full extraction must resolve a stream URL");
    assert_eq!(tracks[0].age_limit, Some(18));
}

#[tokio::test]
async fn playlist_targets_list_entries_lazily() {
    let (extractor, _) = fake_extractor("playlist", None, false);

    let tracks = extractor
        .resolve("https://example.com/playlist?list=PL1", "tester")
        .await
        .expect("resolve");
    assert_eq!(tracks.len(), 3, "all playlist entries should be listed");
    assert!(
        tracks.iter().all(|track| track.stream_url.is_none()),
        "flat playlist entries resolve lazily at play time"
    );
}

#[tokio::test]
async fn age_gate_without_cookies_reports_a_hint() {
    let (extractor, _) = fake_extractor("agegate", None, false);

    let err = extractor.resolve("age-gated thing", "tester").await.expect_err("must fail");
    assert!(matches!(err, ExtractError::AgeRestricted(_)));
    assert!(err.hint().contains("YTDLP_COOKIES"), "hint should point at cookie setup");
}

#[tokio::test]
async fn cookies_unlock_age_gated_content_on_the_first_strategy() {
    let (extractor, log) = fake_extractor("cookies", Some(PathBuf::from("cookies.txt")), false);

    let tracks = extractor.resolve("age-gated thing", "tester").await.expect("resolve");
    assert_eq!(tracks.len(), 1);
    assert_eq!(log_lines(&log), 1, "cookie strategy should succeed immediately");
}

#[tokio::test]
async fn extraction_falls_back_across_strategies() {
    let (extractor, log) = fake_extractor("fallback", None, true);

    let (tracks, strategy) = extractor
        .resolve_detailed("flaky thing", "tester")
        .await
        .expect("resolve after fallback");
    assert_eq!(tracks.len(), 1);
    assert_eq!(log_lines(&log), 2, "first strategy fails, second succeeds");
    assert!(!strategy.is_empty(), "the winning strategy should be reported");
}

#[tokio::test]
async fn unavailable_content_is_not_found() {
    let (extractor, _) = fake_extractor("missing", None, false);

    let err = extractor.resolve("unavailable thing", "tester").await.expect_err("must fail");
    assert!(matches!(err, ExtractError::NotFound(_)));
}

#[tokio::test]
async fn version_is_reported() {
    let (extractor, _) = fake_extractor("version", None, false);

    assert_eq!(extractor.version().await.expect("version"), "2026.01.01-fake");
}

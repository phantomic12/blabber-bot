//! Round-trip tests for the SQLite persistence layer.

use blabber_bot::extractor::TrackMeta;
use blabber_bot::persistence::{Db, PersistedGuild};
use blabber_bot::player::LoopMode;
use std::collections::HashMap;
use std::path::Path;

fn meta(title: &str) -> TrackMeta {
    TrackMeta {
        title: title.to_string(),
        page_url: format!("https://example.com/{title}"),
        thumbnail: None,
        uploader: Some("Fake Artist".to_string()),
        duration: Some(123.0),
        is_live: false,
        age_limit: None,
        stream_url: Some("https://example.com/stream.webm".to_string()),
        http_headers: HashMap::new(),
        protocol: Some("https".to_string()),
        filesize: None,
        resolved_at: None,
        requester: "tester".to_string(),
        requester_id: Some(42),
    }
}

#[tokio::test]
async fn round_trips_guild_state() {
    let db = Db::open(Path::new(":memory:")).expect("open in-memory db");

    let state = PersistedGuild {
        current: Some(meta("now")),
        queue: vec![meta("a"), meta("b")],
        loop_mode: LoopMode::Queue,
        volume: 0.7,
        autoplay: true,
        dj_role: None,
        requester_only: true,
        vote_skip: true,
        vote_skip_ratio: 0.6,
        text_channel: None,
    };

    db.save(42, &state).await.expect("save");
    let loaded = db.load_all().await.expect("load");
    assert_eq!(loaded.len(), 1);
    let (guild_id, restored) = &loaded[0];
    assert_eq!(*guild_id, 42);
    assert_eq!(restored.queue.len(), 2);
    assert_eq!(restored.current.as_ref().map(|meta| meta.title.as_str()), Some("now"));
    assert_eq!(restored.loop_mode, LoopMode::Queue);
    assert_eq!(restored.volume, 0.7);
    assert!(restored.autoplay);
    assert!(restored.requester_only);
    assert!(restored.vote_skip);
    assert_eq!(restored.vote_skip_ratio, 0.6);
    assert_eq!(restored.queue[0].requester_id, Some(42));
}

#[tokio::test]
async fn save_is_an_upsert_and_delete_clears() {
    let db = Db::open(Path::new(":memory:")).expect("open in-memory db");

    let make = |queue: Vec<TrackMeta>| PersistedGuild {
        current: None,
        queue,
        loop_mode: LoopMode::Off,
        volume: 1.0,
        autoplay: false,
        dj_role: None,
        requester_only: false,
        vote_skip: false,
        vote_skip_ratio: 0.5,
        text_channel: None,
    };

    db.save(1, &make(vec![meta("a")])).await.expect("save");
    db.save(1, &make(vec![meta("a"), meta("b")])).await.expect("overwrite");

    let loaded = db.load_all().await.expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].1.queue.len(), 2, "second save should replace the first");

    db.delete(1).await.expect("delete");
    assert!(db.load_all().await.expect("load").is_empty());
}

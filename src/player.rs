//! Per-guild playback state: queue, loop modes, volume, and track lifecycle.
//!
//! Each playing track gets its own one-shot end/error handler (`TrackTerminator`).
//! Commands that stop playback mark the terminator as fired *before* stopping,
//! so the resulting end event is ignored and the command drives the transition
//! itself. Natural ends fall through to [`advance`], which applies loop modes.

use crate::config::Config;
use crate::extractor::{Extractor, TrackMeta};
use crate::format;
use crate::persistence::{Db, PersistedGuild};
use async_trait::async_trait;
use serenity::all::{ChannelId, Context, CreateMessage, GuildId, RoleId, UserId};
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose, HlsRequest, HttpRequest, Input};
use songbird::tracks::{PlayMode, Track, TrackHandle};
use songbird::{Call, Songbird};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoopMode {
    Off,
    Track,
    Queue,
}

impl LoopMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Queue => "queue",
        }
    }
}

/// Why playback moved on from the previous track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceReason {
    /// The track ran to its end.
    Finished,
    /// The track failed to initialise or errored mid-stream.
    Errored,
    /// A user skipped it; never re-plays it, even in track-loop mode.
    Skipped,
}

#[derive(Debug)]
pub struct GuildPlayer {
    pub queue: VecDeque<TrackMeta>,
    pub current: Option<TrackMeta>,
    pub handle: Option<TrackHandle>,
    /// Shared with the current track's terminator; set once the track's
    /// ending has been handled, so a second end/error event is ignored.
    pub current_fired: Arc<AtomicBool>,
    pub loop_mode: LoopMode,
    /// Volume multiplier, `1.0` = 100%.
    pub volume: f32,
    pub paused: bool,
    /// Whether to queue related tracks when the queue runs dry.
    pub autoplay: bool,
    /// Page URLs of recently played tracks, used to avoid replaying them
    /// when autoplay picks related tracks.
    pub history: VecDeque<String>,
    /// Role granting DJ privileges. When set, control commands are restricted.
    pub dj_role: Option<RoleId>,
    /// Whether non-privileged users may only control their own tracks.
    pub requester_only: bool,
    /// Whether non-privileged users skip via a majority vote.
    pub vote_skip: bool,
    /// Fraction of the voice channel that must vote to skip.
    pub vote_skip_ratio: f32,
    /// Users who voted to skip the current track; cleared on track change.
    pub skip_votes: HashSet<UserId>,
    /// Text channel to announce track transitions in.
    pub text_channel: Option<ChannelId>,
}

/// How many recently played page URLs to remember.
const HISTORY_CAP: usize = 64;

impl GuildPlayer {
    fn new(volume: f32, autoplay: bool) -> Self {
        Self {
            queue: VecDeque::new(),
            current: None,
            handle: None,
            current_fired: Arc::new(AtomicBool::new(false)),
            loop_mode: LoopMode::Off,
            volume,
            paused: false,
            autoplay,
            history: VecDeque::new(),
            dj_role: None,
            requester_only: false,
            vote_skip: false,
            vote_skip_ratio: 0.5,
            skip_votes: HashSet::new(),
            text_channel: None,
        }
    }

    /// Snapshot for persistence. Transient state (handles, votes, history) is
    /// intentionally dropped.
    pub fn to_persisted(&self) -> PersistedGuild {
        let mut queue = self.queue.iter().cloned().collect::<Vec<_>>();
        // The interrupted track goes first in line when restored.
        if let Some(current) = self.current.clone() {
            queue.insert(0, current);
        }

        PersistedGuild {
            current: None,
            queue,
            loop_mode: self.loop_mode,
            volume: self.volume,
            autoplay: self.autoplay,
            dj_role: self.dj_role,
            requester_only: self.requester_only,
            vote_skip: self.vote_skip,
            vote_skip_ratio: self.vote_skip_ratio,
            text_channel: self.text_channel,
        }
    }

    /// Rebuilds a player from persistence. The restored player is idle: the
    /// queue starts on the next interaction (e.g. `/play`).
    pub fn from_persisted(state: PersistedGuild) -> Self {
        Self {
            queue: state.queue.into(),
            current: None,
            handle: None,
            current_fired: Arc::new(AtomicBool::new(false)),
            loop_mode: state.loop_mode,
            volume: state.volume,
            paused: false,
            autoplay: state.autoplay,
            history: VecDeque::new(),
            dj_role: state.dj_role,
            requester_only: state.requester_only,
            vote_skip: state.vote_skip,
            vote_skip_ratio: state.vote_skip_ratio,
            skip_votes: HashSet::new(),
            text_channel: state.text_channel,
        }
    }

    /// Decides which track plays after a transition, mutating state
    /// accordingly. Returns `None` when the queue is exhausted.
    pub fn next_after(&mut self, reason: AdvanceReason) -> Option<TrackMeta> {
        self.handle = None;
        self.paused = false;
        let current = self.current.take();

        match reason {
            AdvanceReason::Finished => match self.loop_mode {
                LoopMode::Track => current,
                LoopMode::Queue => {
                    if let Some(finished) = current {
                        self.queue.push_back(finished);
                    }
                    self.queue.pop_front()
                },
                LoopMode::Off => self.queue.pop_front(),
            },
            // Skipped and errored tracks are never re-played.
            AdvanceReason::Skipped | AdvanceReason::Errored => self.queue.pop_front(),
        }
    }

    /// Records a track as played for autoplay history purposes.
    fn note_played(&mut self, page_url: &str) {
        self.history.push_back(page_url.to_string());
        while self.history.len() > HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// Marks the current track's terminator as fired so its end event is a
    /// no-op. Must be called before any command stops or replaces the track.
    pub fn disarm_current(&self) {
        self.current_fired.store(true, Ordering::SeqCst);
    }

    /// Whether nothing is currently playing.
    pub fn is_idle(&self) -> bool {
        self.current.is_none()
    }
}

/// Shared, process-wide state.
pub struct Shared {
    pub config: Arc<Config>,
    pub extractor: Extractor,
    pub songbird: Arc<Songbird>,
    /// Shared HTTP client used to stream audio.
    pub http_client: reqwest::Client,
    /// Persistent queue/settings storage.
    pub db: Arc<Db>,
    pub players: RwLock<HashMap<GuildId, GuildPlayer>>,
}

impl Shared {
    pub fn new(config: Arc<Config>, songbird: Arc<Songbird>, db: Arc<Db>) -> Arc<Self> {
        Arc::new(Self {
            extractor: Extractor::new(&config),
            config,
            songbird,
            http_client: reqwest::Client::new(),
            db,
            players: RwLock::new(HashMap::new()),
        })
    }

    /// Rehydrates players from persisted state at startup.
    pub async fn restore(&self, states: Vec<(u64, PersistedGuild)>) {
        let mut players = self.players.write().await;
        for (guild_id, state) in states {
            players.insert(GuildId::new(guild_id), GuildPlayer::from_persisted(state));
        }
    }

    /// Persists a guild's current state (no-op if it has none).
    pub async fn persist(&self, guild_id: GuildId) {
        let snapshot = {
            let players = self.players.read().await;
            players.get(&guild_id).map(GuildPlayer::to_persisted)
        };

        if let Some(state) = snapshot {
            if let Err(err) = self.db.save(guild_id.get(), &state).await {
                tracing::warn!(%err, %guild_id, "failed to persist guild state");
            }
        }
    }

    /// Removes a guild's player and its persisted state (used by `/leave`).
    pub async fn forget(&self, guild_id: GuildId) {
        if let Some(player) = self.players.write().await.remove(&guild_id) {
            player.disarm_current();
        }
        if let Err(err) = self.db.delete(guild_id.get()).await {
            tracing::warn!(%err, %guild_id, "failed to delete persisted guild state");
        }
    }

    /// Creates the player for a guild if it doesn't exist yet.
    pub async fn ensure_player(&self, guild_id: GuildId) -> tokio::sync::RwLockWriteGuard<'_, HashMap<GuildId, GuildPlayer>> {
        let mut players = self.players.write().await;
        players
            .entry(guild_id)
            .or_insert_with(|| GuildPlayer::new(self.config.default_volume_scale(), self.config.autoplay));
        players
    }

    pub async fn remove_player(&self, guild_id: GuildId) {
        if let Some(player) = self.players.write().await.remove(&guild_id) {
            player.disarm_current();
        }
    }
}

/// A lazily instantiated media stream.
///
/// Resolves a fresh stream URL when the cached one is missing or stale, then
/// hands off to songbird's HTTP/HLS sources. Resolution happens off the audio
/// mixer thread via `create_async`.
pub struct TrackSource {
    meta: TrackMeta,
    shared: Arc<Shared>,
}

#[async_trait]
impl Compose for TrackSource {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let mut meta = self.meta.clone();

        if meta.needs_resolution() {
            let fresh = self
                .shared
                .extractor
                .resolve_one(&meta.page_url, &meta.requester)
                .await
                .map_err(|err| AudioStreamError::Fail(Box::new(err)))?;
            meta = fresh;
            self.meta = meta.clone();
        }

        open_stream(&meta, &self.shared.http_client).await
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

async fn open_stream(
    meta: &TrackMeta,
    client: &reqwest::Client,
) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in &meta.http_headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    let url = meta.stream_url.clone().ok_or_else(|| {
        let err: Box<dyn std::error::Error + Send + Sync> = "track has no stream URL".into();
        AudioStreamError::Fail(err)
    })?;

    if meta.protocol.as_deref() == Some("m3u8_native") {
        HlsRequest::new_with_headers(client.clone(), url, headers).create()
    } else {
        let mut request = HttpRequest {
            client: client.clone(),
            request: url,
            headers,
            content_length: meta.filesize,
        };
        request.create_async().await
    }
}

/// One-shot handler attached to each playing track for its end and error
/// events. Commands disarm it before stopping playback themselves.
pub struct TrackTerminator {
    shared: Arc<Shared>,
    ctx: Context,
    guild_id: GuildId,
    fired: Arc<AtomicBool>,
}

#[async_trait]
impl EventHandler for TrackTerminator {
    async fn act(&self, event: &EventContext<'_>) -> Option<Event> {
        if self.fired.swap(true, Ordering::SeqCst) {
            return None;
        }

        let errored = matches!(
            event,
            EventContext::Track(states)
                if states.iter().any(|(state, _)| matches!(state.playing, PlayMode::Errored(_)))
        );
        let reason = if errored {
            AdvanceReason::Errored
        } else {
            AdvanceReason::Finished
        };

        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let guild_id = self.guild_id;

        tokio::spawn(async move {
            // Capture the ending track before `advance` consumes it; autoplay
            // uses it as the seed for related tracks.
            let seed = shared
                .players
                .read()
                .await
                .get(&guild_id)
                .and_then(|player| player.current.clone());

            if reason == AdvanceReason::Errored {
                if let Some(meta) = &seed {
                    announce(
                        &ctx,
                        &shared,
                        guild_id,
                        &format!("⚠️ Playback of **{}** failed, skipping.", meta.title),
                    )
                    .await;
                }
            }

            let mut started = advance(&ctx, &shared, guild_id, reason).await;

            // The queue ran dry on a natural end: keep the music going with
            // related tracks when autoplay is enabled.
            if started.is_none() && reason == AdvanceReason::Finished {
                if let Some(seed) = &seed {
                    if fill_autoplay(&shared, guild_id, seed).await > 0 {
                        started = advance(&ctx, &shared, guild_id, reason).await;
                    }
                }
            }

            match started {
                Some(meta) => announce_now_playing(&ctx, &shared, guild_id, &meta).await,
                None => {
                    announce(&ctx, &shared, guild_id, "🏁 Queue finished.").await;
                },
            }
        });

        None
    }
}

/// Starts playback of `meta`, replacing whatever is currently playing.
pub async fn play_track(
    ctx: &Context,
    shared: &Arc<Shared>,
    guild_id: GuildId,
    call: &Arc<Mutex<Call>>,
    meta: TrackMeta,
) -> anyhow::Result<()> {
    let volume = {
        let players = shared.players.read().await;
        players.get(&guild_id).map(|p| p.volume).unwrap_or_else(|| shared.config.default_volume_scale())
    };

    let source = TrackSource {
        meta: meta.clone(),
        shared: shared.clone(),
    };
    let track = Track::new(Input::Lazy(Box::new(source))).volume(volume);

    let handle = call.lock().await.play_only(track);

    let fired = Arc::new(AtomicBool::new(false));
    for event in [TrackEvent::End, TrackEvent::Error] {
        let terminator = TrackTerminator {
            shared: shared.clone(),
            ctx: ctx.clone(),
            guild_id,
            fired: fired.clone(),
        };
        let _ = handle.add_event(Event::Track(event), terminator);
    }

    let mut players = shared.ensure_player(guild_id).await;
    let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
    player.disarm_current();
    player.note_played(&meta.page_url);
    player.skip_votes.clear();
    player.current = Some(meta);
    player.handle = Some(handle);
    player.current_fired = fired;
    player.paused = false;

    Ok(())
}

/// Queues related tracks for `seed` when autoplay is on. Returns how many
/// tracks were added.
pub async fn fill_autoplay(shared: &Arc<Shared>, guild_id: GuildId, seed: &TrackMeta) -> usize {
    let history = {
        let players = shared.players.read().await;
        match players.get(&guild_id) {
            Some(player) if player.autoplay => player.history.iter().cloned().collect::<Vec<_>>(),
            _ => return 0,
        }
    };

    let related = shared.extractor.related(seed, &history, &seed.requester).await;
    if related.is_empty() {
        return 0;
    }

    let added = related.len();
    if let Some(player) = shared.players.write().await.get_mut(&guild_id) {
        for track in related {
            player.queue.push_back(track);
        }
    }
    added
}

/// Moves playback along according to `reason`, returning the track that
/// started (if any).
pub async fn advance(
    ctx: &Context,
    shared: &Arc<Shared>,
    guild_id: GuildId,
    reason: AdvanceReason,
) -> Option<TrackMeta> {
    let call = shared.songbird.get(guild_id)?;

    let next = {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id)?;
        player.next_after(reason)
    };

    let Some(meta) = next else {
        shared.persist(guild_id).await;
        return None;
    };

    if let Err(err) = play_track(ctx, shared, guild_id, &call, meta.clone()).await {
        tracing::warn!(%err, "failed to start track");
        // Try the remainder of the queue rather than stalling.
        return Box::pin(advance(ctx, shared, guild_id, AdvanceReason::Errored)).await;
    }

    shared.persist(guild_id).await;
    Some(meta)
}

/// Sends `message` to the guild's announce channel, if one is set.
pub async fn announce(ctx: &Context, shared: &Arc<Shared>, guild_id: GuildId, message: &str) {
    let channel = shared.players.read().await.get(&guild_id).and_then(|p| p.text_channel);
    if let Some(channel) = channel {
        if let Err(err) = channel
            .send_message(ctx.clone(), CreateMessage::new().content(message))
            .await
        {
            tracing::debug!(%err, "failed to announce message");
        }
    }
}

pub async fn announce_now_playing(ctx: &Context, shared: &Arc<Shared>, guild_id: GuildId, meta: &TrackMeta) {
    let mut text = format!("▶️ Now playing **{}**", meta.title);
    if let Some(duration) = meta.duration {
        text.push_str(&format!(" `{}`", format::fmt_secs(Some(duration))));
    }
    if meta.age_restricted() {
        text.push_str(" 🔞");
    }
    if meta.is_live {
        text.push_str(" 🔴");
    }
    if let Some(uploader) = &meta.uploader {
        text.push_str(&format!(" · by {uploader}"));
    }
    text.push_str(&format!(" — requested by {}", meta.requester));

    announce(ctx, shared, guild_id, &text).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn meta(title: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            page_url: format!("https://example.com/{title}"),
            thumbnail: None,
            uploader: Some("Fake Artist".to_string()),
            duration: Some(100.0),
            is_live: false,
            age_limit: None,
            stream_url: Some("https://example.com/stream.webm".to_string()),
            http_headers: HashMap::new(),
            protocol: Some("https".to_string()),
            filesize: None,
            resolved_at: None,
            requester: "tester".to_string(),
            requester_id: Some(7),
        }
    }

    fn player_with(current: &str, queued: &[&str]) -> GuildPlayer {
        let mut player = GuildPlayer::new(1.0, false);
        player.current = Some(meta(current));
        for title in queued {
            player.queue.push_back(meta(title));
        }
        player
    }

    fn queue_titles(player: &GuildPlayer) -> Vec<&str> {
        player.queue.iter().map(|track| track.title.as_str()).collect()
    }

    #[test]
    fn off_mode_advances_through_the_queue() {
        let mut player = player_with("a", &["b", "c"]);
        assert_eq!(player.next_after(AdvanceReason::Finished).unwrap().title, "b");
        assert_eq!(queue_titles(&player), &["c"]);
    }

    #[test]
    fn track_loop_replays_the_current_track() {
        let mut player = player_with("a", &["b"]);
        player.loop_mode = LoopMode::Track;
        assert_eq!(player.next_after(AdvanceReason::Finished).unwrap().title, "a");
        assert_eq!(queue_titles(&player), &["b"]);
    }

    #[test]
    fn queue_loop_requeues_the_finished_track() {
        let mut player = player_with("a", &["b"]);
        player.loop_mode = LoopMode::Queue;
        assert_eq!(player.next_after(AdvanceReason::Finished).unwrap().title, "b");
        assert_eq!(queue_titles(&player), &["a"]);
    }

    #[test]
    fn skip_never_replays_the_current_track() {
        let mut player = player_with("a", &["b"]);
        player.loop_mode = LoopMode::Track;
        assert_eq!(player.next_after(AdvanceReason::Skipped).unwrap().title, "b");
        assert!(player.queue.is_empty());
    }

    #[test]
    fn errored_track_is_dropped() {
        let mut player = player_with("a", &[]);
        player.loop_mode = LoopMode::Track;
        assert!(player.next_after(AdvanceReason::Errored).is_none());
    }

    #[test]
    fn empty_queue_ends_the_session() {
        let mut player = player_with("a", &[]);
        assert!(player.next_after(AdvanceReason::Finished).is_none());
        assert!(player.is_idle());
    }

    #[test]
    fn persistence_round_trip_restores_a_workable_queue() {
        let mut player = player_with("a", &["b", "c"]);
        player.loop_mode = LoopMode::Queue;
        player.volume = 0.5;
        player.autoplay = true;

        let restored = GuildPlayer::from_persisted(player.to_persisted());

        // The interrupted track goes first in line; the player starts idle so
        // the queue resumes on the next interaction.
        assert_eq!(queue_titles(&restored), &["a", "b", "c"]);
        assert!(restored.is_idle());
        assert_eq!(restored.loop_mode, LoopMode::Queue);
        assert_eq!(restored.volume, 0.5);
        assert!(restored.autoplay);
    }

    #[test]
    fn played_history_is_capped() {
        let mut player = player_with("a", &[]);
        for i in 0..(HISTORY_CAP + 10) {
            player.note_played(&format!("https://example.com/{i}"));
        }
        assert_eq!(player.history.len(), HISTORY_CAP);
    }
}

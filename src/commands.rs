//! Slash command definitions, startup registration, and handlers.

use crate::extractor::TrackMeta;
use crate::format;
use crate::player::{self, AdvanceReason, GuildPlayer, LoopMode, Shared};
use serenity::all::{
    Command, CommandInteraction, CommandOptionType, Context, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, EditInteractionResponse, GuildId,
    Http, Permissions, ResolvedOption, ResolvedValue,
};
use songbird::Call;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Builds this bot's slash command set.
pub fn definitions() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("play")
            .description("Play a song, URL, or playlist (age-restricted videos supported)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "query",
                    "Search terms, a video/track URL, or a playlist URL",
                )
                .required(true),
            ),
        CreateCommand::new("skip").description("Skip the current track"),
        CreateCommand::new("skipto")
            .description("Skip to a track in the queue, dropping everything before it")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "position", "Queue position (1 = next track)")
                    .required(true)
                    .min_int_value(1),
            ),
        CreateCommand::new("stop").description("Stop playback and clear the queue"),
        CreateCommand::new("pause").description("Pause playback"),
        CreateCommand::new("resume").description("Resume playback"),
        CreateCommand::new("seek")
            .description("Seek to a position in the current track")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "position", "Timestamp: 90, 1:30 or 1:02:03")
                    .required(true),
            ),
        CreateCommand::new("volume")
            .description("Set the playback volume")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "level", "Volume, 0–200%")
                    .required(true)
                    .min_int_value(0)
                    .max_int_value(200),
            ),
        CreateCommand::new("queue").description("Show the queue").add_option(
            CreateCommandOption::new(CommandOptionType::Integer, "page", "Page number").min_int_value(1),
        ),
        CreateCommand::new("nowplaying").description("Show the current track"),
        CreateCommand::new("shuffle").description("Shuffle the queue"),
        CreateCommand::new("loop")
            .description("Set the loop mode")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "mode", "What to loop")
                    .required(true)
                    .add_string_choice("Off", "off")
                    .add_string_choice("Track", "track")
                    .add_string_choice("Queue", "queue"),
            ),
        CreateCommand::new("remove")
            .description("Remove a track from the queue")
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "position", "Queue position")
                    .required(true)
                    .min_int_value(1),
            ),
        CreateCommand::new("clear").description("Clear the queue"),
        CreateCommand::new("autoplay")
            .description("Toggle autoplay of related tracks when the queue runs dry")
            .add_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "enabled",
                "On or off (toggles when omitted)",
            )),
        CreateCommand::new("djrole")
            .description("Set the DJ role that unlocks control commands (omit role to clear)")
            .add_option(CreateCommandOption::new(
                CommandOptionType::Role,
                "role",
                "The DJ role; omit to clear",
            )),
        CreateCommand::new("requestersonly")
            .description("Restrict playback control to each track's requester (and DJs)")
            .add_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "enabled",
                "On or off (toggles when omitted)",
            )),
        CreateCommand::new("voteskip")
            .description("Let non-DJs vote to skip")
            .add_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "enabled",
                "On or off (toggles when omitted)",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "threshold",
                    "Percent of the voice channel that must vote",
                )
                .min_int_value(10)
                .max_int_value(100),
            ),
        CreateCommand::new("settings").description("Show this server's playback settings"),
        CreateCommand::new("diagnose")
            .description("Preflight: yt-dlp, JS runtime, cookies, and a live extraction probe")
            .add_option(CreateCommandOption::new(
                CommandOptionType::String,
                "url",
                "Video URL to probe (e.g. an age-restricted one)",
            )),
        CreateCommand::new("leave").description("Disconnect the bot from voice"),
        CreateCommand::new("refreshcommands")
            .description("Re-sync this bot's slash commands with Discord (Manage Server)")
            .default_member_permissions(Permissions::MANAGE_GUILD),
    ]
}

/// Replaces the command set, returning how many commands are now live.
///
/// `PUT /applications/{id}/guilds/{guild}/commands` (and its global twin) is a
/// *bulk overwrite*: it replaces the whole set in one request. So there is no
/// reason to delete commands first — doing that costs one request per command
/// against a 5-per-20s bucket, which is how a 21-command set once blacked the
/// bot out for 85 seconds on every restart. Sync once, atomic, done.
pub async fn sync(http: &Http, guild_id: Option<GuildId>) -> anyhow::Result<usize> {
    let commands = definitions();
    let count = commands.len();

    match guild_id {
        Some(guild_id) => {
            http.create_guild_commands(guild_id, &commands).await?;
            tracing::info!(count, %guild_id, "synced slash commands to guild");
        }
        None => {
            http.create_global_commands(&commands).await?;
            tracing::info!(count, "synced global slash commands");
        }
    }

    Ok(count)
}

/// Removes every command of the scope that is *not* in this bot's set.
///
/// The bulk overwrite in [`sync`] already drops stale commands, so this is only
/// needed when migrating away from a scope — e.g. clearing guild commands that
/// were left in a previous home guild, or global commands superseded by
/// guild-scoped ones. Commands that are still defined are left alone.
pub async fn prune_stale(http: &Http, keep_guild: Option<GuildId>) -> anyhow::Result<usize> {
    let mut removed = 0;

    // Global commands would duplicate the guild-scoped set, so they always go.
    for command in &http.get_global_commands().await? {
        http.delete_global_command(command.id).await?;
        removed += 1;
    }

    // Guilds other than the configured home shouldn't carry this bot's
    // commands at all — a leftover set there shows up as ghost commands.
    for guild in &http.get_guilds(None, Some(200)).await? {
        if Some(guild.id) == keep_guild {
            continue;
        }
        let commands = http.get_guild_commands(guild.id).await?;
        for command in &commands {
            http.delete_guild_command(guild.id, command.id).await?;
            removed += 1;
        }
        if !commands.is_empty() {
            tracing::info!(guild = %guild.id, count = commands.len(), "pruned commands from other guild");
        }
    }

    Ok(removed)
}

type CmdResult = Result<String, String>;

async fn run_command(
    ctx: &Context,
    shared: &Arc<Shared>,
    cmd: &CommandInteraction,
    name: &str,
) -> CmdResult {
    match name {
        "play" => play(ctx, shared, cmd).await,
        "skipto" => skipto(ctx, shared, cmd).await,
        "stop" => stop(shared, cmd).await,
        "pause" => set_paused(shared, cmd, true).await,
        "resume" => set_paused(shared, cmd, false).await,
        "seek" => seek(shared, cmd).await,
        "volume" => volume(shared, cmd).await,
        "queue" => queue(shared, cmd).await,
        "nowplaying" => nowplaying(shared, cmd).await,
        "shuffle" => shuffle(shared, cmd).await,
        "loop" => set_loop(shared, cmd).await,
        "remove" => remove(shared, cmd).await,
        "clear" => clear(shared, cmd).await,
        "autoplay" => autoplay(shared, cmd).await,
        "djrole" => dj_role_set(shared, cmd).await,
        "requestersonly" => set_requester_only(shared, cmd).await,
        "voteskip" => set_voteskip(shared, cmd).await,
        "settings" => settings(shared, cmd).await,
        "diagnose" => diagnose(shared, cmd).await,
        "leave" => leave(shared, cmd).await,
        "refreshcommands" => refresh_commands(ctx, shared, cmd).await,
        "skip" => skip(ctx, shared, cmd).await,
        other => Err(format!("unknown command '{other}'")),
    }
}

/// Entry point for slash command interactions.
pub async fn dispatch(ctx: &Context, shared: &Arc<Shared>, cmd: &CommandInteraction) -> Result<(), String> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
    )
    .await
    .map_err(|err| format!("failed to acknowledge command: {err}"))?;

    let name = cmd.data.name.as_str();
    let control = match name {
        "skip" => Some(Control::Skip),
        "pause" | "resume" | "seek" => Some(Control::Track),
        "skipto" | "stop" | "volume" | "shuffle" | "loop" | "remove" | "clear" | "leave" => {
            Some(Control::Queue)
        },
        _ => None,
    };

    let result = match control {
        Some(Control::Skip) => match check_access(shared, cmd, Control::Skip).await {
            Ok(Access::Vote) => cast_skip_vote(ctx, shared, cmd).await,
            Ok(Access::Allow) => skip(ctx, shared, cmd).await,
            Err(err) => Err(err),
        },
        Some(control) => match check_access(shared, cmd, control).await {
            Ok(_) => run_command(ctx, shared, cmd, name).await,
            Err(err) => Err(err),
        },
        None => run_command(ctx, shared, cmd, name).await,
    };

    let content = result.unwrap_or_else(|err| format!("⚠️ {err}"));
    cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(content))
        .await
        .map_err(|err| format!("failed to edit response: {err}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn play(ctx: &Context, shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let query = option_str(&opts, "query").ok_or("give me a song name or URL")?;
    let guild_id = guild_of(cmd)?;
    let channel_id = caller_voice_channel(ctx, cmd)?;
    let requester = display_name(cmd);

    let mut tracks = shared
        .extractor
        .resolve(&query, &requester)
        .await
        .map_err(|err| {
            let hint = err.hint();
            if hint.is_empty() {
                err.to_string()
            } else {
                format!("{err}. {hint}")
            }
        })?;
    for track in &mut tracks {
        track.requester_id = Some(cmd.user.id.get());
    }

    let total = tracks.len();
    let first_title = tracks[0].title.clone();

    let to_start = {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
        player.text_channel = Some(cmd.channel_id);

        for track in tracks {
            player.queue.push_back(track);
        }

        if player.is_idle() {
            player.queue.pop_front()
        } else {
            None
        }
    };

    // A `/play` means somebody is in the channel and listening, so any idle
    // countdown from a previous emptied channel is obsolete.
    shared.cancel_empty_timer(guild_id).await;

    let reply = match to_start {
        Some(meta) => {
            let call = ensure_call(shared, guild_id, channel_id).await?;
            player::play_track(ctx, shared, guild_id, &call, meta.clone())
                .await
                .map_err(|err| format!("couldn't start playback: {err}"))?;
            format!("▶️ Now playing {}", describe(&meta))
        },
        None => {
            let position = {
                let players = shared.players.read().await;
                players.get(&guild_id).map(|p| p.queue.len()).unwrap_or(0)
            };
            if total == 1 {
                format!("✅ Queued **{first_title}** — position **{position}**")
            } else {
                format!(
                    "✅ Queued **{total}** tracks — positions **1–{position}** (next up: **{first_title}**)"
                )
            }
        },
    };

    shared.persist(guild_id).await;
    Ok(reply)
}

async fn skip(ctx: &Context, shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    caller_voice_channel(ctx, cmd)?;
    let (_call, skipped_title) = stop_current(shared, guild_id).await?;

    let next = player::advance(ctx, shared, guild_id, AdvanceReason::Skipped).await;
    Ok(match next {
        Some(next) => format!("⏭️ Skipped **{skipped_title}** — now playing {}", describe(&next)),
        None => format!("⏭️ Skipped **{skipped_title}**. The queue is empty."),
    })
}

async fn skipto(ctx: &Context, shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let position = option_int(&opts, "position").unwrap_or(1).max(1) as usize;
    let guild_id = guild_of(cmd)?;
    caller_voice_channel(ctx, cmd)?;

    {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;

        if position > player.queue.len() {
            return Err(format!(
                "there are only **{}** track(s) queued",
                player.queue.len()
            ));
        }
        // Drop everything before the target position; the target becomes next.
        player.queue.drain(..position - 1);
        player.disarm_current();
        player.current = None;
        player.handle = None;
        player.paused = false;
    }

    let call = shared
        .songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel")?;
    call.lock().await.stop();

    let next = player::advance(ctx, shared, guild_id, AdvanceReason::Skipped).await;
    Ok(match next {
        Some(meta) => format!("⏭️ Now playing {}", describe(&meta)),
        None => "⏭️ The queue is empty.".to_string(),
    })
}

async fn stop(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;

        if player.is_idle() && player.queue.is_empty() {
            return Err("nothing is playing".to_string());
        }

        player.disarm_current();
        player.queue.clear();
        player.current = None;
        player.handle = None;
        player.paused = false;
    }

    let call = shared
        .songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel")?;
    call.lock().await.stop();
    shared.persist(guild_id).await;

    Ok("⏹️ Stopped playback and cleared the queue.".to_string())
}

async fn set_paused(shared: &Arc<Shared>, cmd: &CommandInteraction, pause: bool) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    let mut players = shared.players.write().await;
    let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;

    let title = player
        .current
        .as_ref()
        .map(|meta| meta.title.clone())
        .ok_or("nothing is playing")?;

    let handle = player.handle.as_ref().ok_or("nothing is playing")?;
    if pause {
        handle.pause().map_err(|err| format!("couldn't pause: {err}"))?;
        player.paused = true;
        Ok(format!("⏸️ Paused **{title}**."))
    } else {
        handle.play().map_err(|err| format!("couldn't resume: {err}"))?;
        player.paused = false;
        Ok(format!("▶️ Resumed **{title}**."))
    }
}

async fn seek(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let raw = option_str(&opts, "position").ok_or("give me a timestamp, e.g. `1:30`")?;
    let position = format::parse_timestamp(&raw)?;

    let guild_id = guild_of(cmd)?;
    let mut players = shared.players.write().await;
    let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;

    let meta = player.current.as_ref().ok_or("nothing is playing")?;
    if meta.is_live {
        return Err("can't seek within a live stream".to_string());
    }
    if let Some(duration) = meta.duration {
        if position as f64 > duration + 2.0 {
            return Err(format!(
                "that's past the end of the track (`{}`)",
                format::fmt_secs(Some(duration))
            ));
        }
    }

    let handle = player.handle.as_ref().ok_or("nothing is playing")?;
    handle
        .seek_async(std::time::Duration::from_secs(position))
        .await
        .map_err(|err| format!("couldn't seek: {err}"))?;

    Ok(format!("⏩ Seeked to **{}**.", format::fmt_count(position)))
}

async fn volume(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let level = option_int(&opts, "level").unwrap_or(100).clamp(0, 200);
    let guild_id = guild_of(cmd)?;

    {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
        player.volume = level as f32 / 100.0;

        if let Some(handle) = &player.handle {
            let _ = handle.set_volume(player.volume);
        }
    }
    shared.persist(guild_id).await;

    Ok(format!("🔊 Volume set to **{level}%**."))
}

async fn queue(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    const PER_PAGE: usize = 10;

    let opts = cmd.data.options();
    let page = option_int(&opts, "page").unwrap_or(1).max(1) as usize;
    let guild_id = guild_of(cmd)?;

    let players = shared.players.read().await;
    let player = players.get(&guild_id).ok_or("the queue is empty")?;

    if player.is_idle() && player.queue.is_empty() {
        return Err("the queue is empty".to_string());
    }

    let total_entries = usize::from(!player.is_idle()) + player.queue.len();
    let pages = total_entries.div_ceil(PER_PAGE).max(1);
    if page > pages {
        return Err(format!("there are only **{pages}** page(s)"));
    }

    let mut lines = Vec::new();
    if let Some(current) = &player.current {
        let mut line = format!("▶️ **Now:** {}", describe(current));
        if player.paused {
            line.push_str(" ⏸️");
        }
        lines.push(line);
    }

    let start = (page - 1) * PER_PAGE;
    let shown = player.queue.iter().skip(start).take(PER_PAGE);
    for (offset, meta) in shown.enumerate() {
        let position = start + offset + 1;
        lines.push(format!("**{position}.** {} — {}", describe(meta), meta.requester));
    }

    if lines.is_empty() {
        lines.push("_The queue is empty._".to_string());
    }

    lines.push(format!(
        "🔁 loop: **{}** · 🔊 **{}%** · page **{page}/{pages}**",
        player.loop_mode.label(),
        (player.volume * 100.0).round() as u32,
    ));

    Ok(lines.join("\n"))
}

async fn nowplaying(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    let players = shared.players.read().await;
    let player = players.get(&guild_id).ok_or("nothing is playing")?;
    let meta = player.current.as_ref().ok_or("nothing is playing")?;
    let handle = player.handle.as_ref().ok_or("nothing is playing")?;

    let state = handle.get_info().await.map_err(|err| format!("couldn't read playback state: {err}"))?;

    let mut lines = vec![format!(
        "{} {}",
        if player.paused { "⏸️" } else { "▶️" },
        describe(meta)
    )];

    let bar = format::progress_bar(state.position, meta.duration);
    if !bar.is_empty() {
        lines.push(bar);
    } else {
        lines.push(format!("**{}** elapsed", format::fmt_count(state.position.as_secs())));
    }

    let source = meta
        .uploader
        .as_ref()
        .map(|uploader| format!("by {uploader}"))
        .unwrap_or_else(|| "unknown source".to_string());
    lines.push(format!(
        "{source} · requested by {} · 🔊 **{}%** · 🔁 loop: **{}**",
        meta.requester,
        (state.volume * 100.0).round() as u32,
        player.loop_mode.label(),
    ));

    // A bare URL is unfurled by Discord into an inline thumbnail.
    if let Some(thumbnail) = &meta.thumbnail {
        lines.push(thumbnail.clone());
    }

    Ok(lines.join("\n"))
}

async fn shuffle(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    use rand::seq::SliceRandom;

    let guild_id = guild_of(cmd)?;
    let shuffled = {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("the queue is empty")?;

        if player.queue.len() < 2 {
            return Err("the queue needs at least two tracks to shuffle".to_string());
        }

        player.queue.make_contiguous().shuffle(&mut rand::rng());
        player.queue.len()
    };
    shared.persist(guild_id).await;

    Ok(format!("🔀 Shuffled **{shuffled}** tracks."))
}

async fn set_loop(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let mode = option_str(&opts, "mode");
    let mode = match mode.as_deref() {
        Some("track") => LoopMode::Track,
        Some("queue") => LoopMode::Queue,
        _ => LoopMode::Off,
    };

    let guild_id = guild_of(cmd)?;
    {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
        player.loop_mode = mode;
    }
    shared.persist(guild_id).await;

    Ok(match mode {
        LoopMode::Off => "🔁 Looping disabled.".to_string(),
        LoopMode::Track => "🔁 Looping the **current track**.".to_string(),
        LoopMode::Queue => "🔁 Looping the **whole queue**.".to_string(),
    })
}

async fn remove(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let position = option_int(&opts, "position").unwrap_or(1).max(1) as usize;
    let guild_id = guild_of(cmd)?;

    let removed = {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("the queue is empty")?;

        player
            .queue
            .remove(position - 1)
            .map(|meta| format!("🗑️ Removed **{}**.", meta.title))
            .ok_or_else(|| format!("no track at position **{position}**"))?
    };
    shared.persist(guild_id).await;

    Ok(removed)
}

async fn clear(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    let cleared = {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("the queue is already empty")?;

        let cleared = player.queue.len();
        if cleared == 0 {
            return Err("the queue is already empty".to_string());
        }
        player.queue.clear();
        cleared
    };
    shared.persist(guild_id).await;

    Ok(format!("🧹 Cleared **{cleared}** track(s) from the queue."))
}

async fn autoplay(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let opts = cmd.data.options();
    let guild_id = guild_of(cmd)?;

    let reply = {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");

        player.autoplay = option_bool(&opts, "enabled").unwrap_or(!player.autoplay);
        format!(
            "🎲 Autoplay is now **{}** — related tracks play when the queue runs dry.",
            on_off(player.autoplay)
        )
    };
    shared.persist(guild_id).await;

    Ok(reply)
}

async fn leave(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;

    if shared.songbird.get(guild_id).is_none() {
        return Err("I'm not connected to a voice channel".to_string());
    }

    shared.cancel_empty_timer(guild_id).await;
    shared.forget(guild_id).await;
    shared
        .songbird
        .remove(guild_id)
        .await
        .map_err(|err| format!("couldn't leave the voice channel: {err}"))?;

    Ok("👋 Left the voice channel.".to_string())
}

/// `/diagnose` — preflight checks plus a live extraction probe.
async fn diagnose(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    use std::time::Instant;

    let opts = cmd.data.options();
    let probe_url = option_str(&opts, "url");
    let mut lines = vec!["🩺 **Preflight diagnostics**".to_string()];

    match shared.extractor.version().await {
        Ok(version) => lines.push(format!("yt-dlp: `{version}` ✅")),
        Err(err) => {
            lines.push(format!("yt-dlp: ❌ {err}"));
            let hint = err.hint();
            if !hint.is_empty() {
                lines.push(format!("_{hint}_"));
            }
        },
    }

    match crate::extractor::js_runtime().await {
        Some(runtime) => {
            lines.push(format!("JS runtime: {runtime} ✅ (needed by yt-dlp for YouTube)"));
        },
        None => lines.push(
            "JS runtime: ❌ _none found — install Deno or Node.js 20+, or YouTube extraction will be limited_"
                .to_string(),
        ),
    }

    lines.push(shared.extractor.cookie_status());

    let started = Instant::now();
    match shared.extractor.resolve("never gonna give you up", "diagnose").await {
        Ok(tracks) => lines.push(format!(
            "Smoke test: ✅ resolved **{}** in {:.1}s",
            tracks[0].title,
            started.elapsed().as_secs_f64()
        )),
        Err(err) => lines.push(format!("Smoke test: ❌ {err}")),
    }

    match probe_url {
        Some(url) => {
            let started = Instant::now();
            match shared.extractor.resolve_detailed(&url, "diagnose").await {
                Ok((tracks, strategy)) => lines.push(format!(
                    "Probe: ✅ unlocked via **{strategy}** in {:.1}s — {}",
                    started.elapsed().as_secs_f64(),
                    tracks[0].title
                )),
                Err(err) => {
                    lines.push(format!("Probe: ❌ {err}"));
                    let hint = err.hint();
                    if !hint.is_empty() {
                        lines.push(format!("_{hint}_"));
                    }
                },
            }
        },
        None => lines.push(
            "_Tip: run `/diagnose url:<age-restricted link>` to verify the age-restricted path._"
                .to_string(),
        ),
    }

    Ok(lines.join("\n"))
}

/// Re-pushes the command definitions to Discord without a restart.
///
/// Useful when the bot's command set changed but the container is still
/// running the old registration, or when slash commands have gone stale in the
/// client (Discord caches them per-scope and per-client for a while).
async fn refresh_commands(
    ctx: &Context,
    _shared: &Arc<Shared>,
    cmd: &CommandInteraction,
) -> CmdResult {
    require_manage_guild(cmd)?;
    let guild_id = guild_of(cmd)?;

    // `ctx.http` carries the application id the gateway set up, so no extra
    // auth round-trip is needed here.
    let count = sync(&ctx.http, Some(guild_id))
        .await
        .map_err(|err| format!("failed to sync slash commands: {err}"))?;

    // Prune anything left in other guilds (e.g. a previous home guild) so the
    // command list can't appear twice.
    let pruned = prune_stale(&ctx.http, Some(guild_id)).await.unwrap_or(0);

    Ok(format!(
        "🔄 **{count}** command(s) synced to this server.{}_Reloading your Discord client (Ctrl+R) refreshes the local cache if they still look stale._",
        if pruned > 0 {
            format!(" Removed {pruned} stale command(s) from other scopes.")
        } else {
            String::new()
        }
    ))
}

async fn dj_role_set(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    require_manage_guild(cmd)?;
    let opts = cmd.data.options();
    let guild_id = guild_of(cmd)?;

    let role = opts.iter().find(|opt| opt.name == "role").and_then(|opt| match opt.value {
        ResolvedValue::Role(role) => Some(role.id),
        _ => None,
    });

    {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
        player.dj_role = role;
    }
    shared.persist(guild_id).await;

    Ok(match role {
        Some(role) => format!(
            "🎧 DJ role set to <@&{role}> — control commands now require it (or **Manage Server**)."
        ),
        None => "🎧 DJ role cleared — the bot is open to everyone again.".to_string(),
    })
}

async fn set_requester_only(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    require_manage_guild(cmd)?;
    let opts = cmd.data.options();
    let guild_id = guild_of(cmd)?;

    let reply = {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");
        player.requester_only = option_bool(&opts, "enabled").unwrap_or(!player.requester_only);
        format!("🔒 Requester-only mode is now **{}**.", on_off(player.requester_only))
    };
    shared.persist(guild_id).await;

    Ok(reply)
}

async fn set_voteskip(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    require_manage_guild(cmd)?;
    let opts = cmd.data.options();
    let guild_id = guild_of(cmd)?;

    let reply = {
        let mut players = shared.ensure_player(guild_id).await;
        let player = players.get_mut(&guild_id).expect("ensure_player inserts entry");

        player.vote_skip = option_bool(&opts, "enabled").unwrap_or(!player.vote_skip);
        if let Some(threshold) = option_int(&opts, "threshold") {
            player.vote_skip_ratio = threshold.clamp(10, 100) as f32 / 100.0;
        }

        format!(
            "🗳️ Vote-skip is now **{}** — {}% of the voice channel must vote to skip.",
            on_off(player.vote_skip),
            (player.vote_skip_ratio * 100.0).round() as u32,
        )
    };
    shared.persist(guild_id).await;

    Ok(reply)
}

async fn settings(shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    let players = shared.players.read().await;

    let Some(player) = players.get(&guild_id) else {
        return Ok("**Server settings**\n_Using defaults — nothing configured yet._".to_string());
    };

    Ok(format!(
        "**Server settings**\n🎧 DJ role: {}\n🔒 Requester-only: **{}**\n🗳️ Vote-skip: **{}** ({}% threshold)\n🎲 Autoplay: **{}**\n🔊 Volume: **{}%**\n🔁 Loop: **{}**\n📝 Queue: **{}** track(s)",
        player
            .dj_role
            .map(|role| format!("<@&{role}>"))
            .unwrap_or_else(|| "_none — open bot_".to_string()),
        on_off(player.requester_only),
        on_off(player.vote_skip),
        (player.vote_skip_ratio * 100.0).round() as u32,
        on_off(player.autoplay),
        (player.volume * 100.0).round() as u32,
        player.loop_mode.label(),
        player.queue.len(),
    ))
}

/// Registers a skip vote, performing the skip once enough votes are in.
async fn cast_skip_vote(ctx: &Context, shared: &Arc<Shared>, cmd: &CommandInteraction) -> CmdResult {
    let guild_id = guild_of(cmd)?;
    caller_voice_channel(ctx, cmd)?;

    let (votes, needed) = {
        let mut players = shared.players.write().await;
        let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;
        if player.is_idle() {
            return Err("nothing is playing".to_string());
        }

        player.skip_votes.insert(cmd.user.id);
        let needed = vote_threshold(voice_voter_count(ctx, guild_id), player.vote_skip_ratio);
        (player.skip_votes.len(), needed)
    };

    if votes < needed {
        return Ok(format!("🗳️ Vote to skip: **{votes}/{needed}** — run `/skip` to vote."));
    }

    let (_call, title) = stop_current(shared, guild_id).await?;
    let next = player::advance(ctx, shared, guild_id, AdvanceReason::Skipped).await;
    Ok(match next {
        Some(next) => format!(
            "🗳️ Vote passed ({votes}/{needed}) — skipped **{title}**. Now playing {}",
            describe(&next)
        ),
        None => format!("🗳️ Vote passed ({votes}/{needed}) — skipped **{title}**. The queue is empty."),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Playback-control commands fall into three access classes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    /// Acts on the current track only.
    Track,
    /// Affects the shared queue or session.
    Queue,
    /// `/skip`, which may be satisfied by a vote.
    Skip,
}

/// How an access check resolved for a non-privileged user.
enum Access {
    Allow,
    Vote,
}

/// Resolves whether the invoking user may run a control command; errors with
/// a user-facing message when they may not.
async fn check_access(
    shared: &Arc<Shared>,
    cmd: &CommandInteraction,
    control: Control,
) -> Result<Access, String> {
    let guild_id = guild_of(cmd)?;
    let players = shared.players.read().await;
    let Some(player) = players.get(&guild_id) else {
        return Ok(Access::Allow); // nothing configured yet: open bot
    };

    if is_privileged(cmd, player) {
        return Ok(Access::Allow);
    }

    // The requester of the current track may always control that track.
    let is_requester = player
        .current
        .as_ref()
        .and_then(|meta| meta.requester_id)
        .is_some_and(|id| id == cmd.user.id.get());
    if is_requester && matches!(control, Control::Track | Control::Skip) {
        return Ok(Access::Allow);
    }

    if control == Control::Skip && player.vote_skip {
        return Ok(Access::Vote);
    }

    if player.dj_role.is_none() && !player.requester_only {
        return Ok(Access::Allow);
    }

    Err(if player.dj_role.is_some() {
        "🔒 DJ only — you need the DJ role (or **Manage Server**) for that.".to_string()
    } else {
        "🔒 Only the track's requester can do that.".to_string()
    })
}

fn is_privileged(cmd: &CommandInteraction, player: &GuildPlayer) -> bool {
    let Some(member) = &cmd.member else {
        return false;
    };

    if let Some(permissions) = member.permissions {
        if permissions.contains(Permissions::ADMINISTRATOR)
            || permissions.contains(Permissions::MANAGE_GUILD)
        {
            return true;
        }
    }

    player.dj_role.is_some_and(|role| member.roles.contains(&role))
}

/// Number of non-bot members in the bot's voice channel — the voting body.
fn voice_voter_count(ctx: &Context, guild_id: GuildId) -> usize {
    let Some(guild) = ctx.cache.guild(guild_id) else {
        return 1;
    };
    let bot_id = ctx.cache.current_user().id;
    let Some(bot_channel) = guild.voice_states.get(&bot_id).and_then(|state| state.channel_id)
    else {
        return 1;
    };

    guild
        .voice_states
        .values()
        .filter(|state| state.channel_id == Some(bot_channel))
        .filter(|state| {
            state.user_id != bot_id && ctx.cache.user(state.user_id).is_none_or(|user| !user.bot)
        })
        .count()
        .max(1)
}

/// Votes needed to pass a skip, given the channel size and threshold ratio.
fn vote_threshold(voters: usize, ratio: f32) -> usize {
    ((voters as f32 * ratio).ceil() as usize).clamp(1, voters.max(1))
}

fn require_manage_guild(cmd: &CommandInteraction) -> Result<(), String> {
    let allowed = cmd.member.as_ref().is_some_and(|member| {
        member.permissions.is_some_and(|permissions| {
            permissions.contains(Permissions::ADMINISTRATOR)
                || permissions.contains(Permissions::MANAGE_GUILD)
        })
    });

    allowed
        .then_some(())
        .ok_or_else(|| "🔒 You need **Manage Server** to change these settings.".to_string())
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Stops the current track — disarming its terminator first so the end event
/// can't trigger a second transition — and returns the call and track title.
async fn stop_current(
    shared: &Arc<Shared>,
    guild_id: GuildId,
) -> Result<(Arc<Mutex<Call>>, String), String> {
    let call = shared
        .songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel")?;

    let mut players = shared.players.write().await;
    let player = players.get_mut(&guild_id).ok_or("nothing is playing")?;

    if player.current.is_none() {
        return Err("nothing is playing".to_string());
    }

    let title = player
        .current
        .as_ref()
        .map(|meta| meta.title.clone())
        .unwrap_or_else(|| "the current track".to_string());

    player.disarm_current();
    player.current = None;
    player.handle = None;
    player.paused = false;
    drop(players);

    call.lock().await.stop();
    Ok((call, title))
}

async fn ensure_call(
    shared: &Arc<Shared>,
    guild_id: GuildId,
    channel_id: serenity::all::ChannelId,
) -> Result<Arc<Mutex<Call>>, String> {
    shared
        .songbird
        .join(guild_id, channel_id)
        .await
        .map_err(|err| format!("couldn't join your voice channel: {err}"))
}

fn guild_of(cmd: &CommandInteraction) -> Result<GuildId, String> {
    cmd.guild_id.ok_or_else(|| "this command only works in a server".to_string())
}

/// The voice channel the invoking user is in.
fn caller_voice_channel(ctx: &Context, cmd: &CommandInteraction) -> Result<serenity::all::ChannelId, String> {
    let guild_id = guild_of(cmd)?;
    let guild = ctx
        .cache
        .guild(guild_id)
        .ok_or("I can't see this server yet — try again in a few seconds")?;

    guild
        .voice_states
        .get(&cmd.user.id)
        .and_then(|state| state.channel_id)
        .ok_or_else(|| "you need to be in a voice channel first".to_string())
}

fn display_name(cmd: &CommandInteraction) -> String {
    cmd.member
        .as_ref()
        .and_then(|member| member.nick.clone())
        .unwrap_or_else(|| cmd.user.name.clone())
}

/// Bold, hyperlinked track description with duration and content badges.
fn describe(meta: &TrackMeta) -> String {
    let mut text = format!("**[{}]({})**", truncate(&meta.title, 80), meta.page_url);
    if let Some(duration) = meta.duration {
        text.push_str(&format!(" `{}`", format::fmt_secs(Some(duration))));
    }
    if meta.age_restricted() {
        text.push_str(" 🔞");
    }
    if meta.is_live {
        text.push_str(" 🔴");
    }
    text
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn option_str<'a>(opts: &'a [ResolvedOption<'a>], name: &str) -> Option<String> {
    opts.iter()
        .find(|opt| opt.name == name)
        .and_then(|opt| match opt.value {
            ResolvedValue::String(value) => Some(value.to_string()),
            _ => None,
        })
}

fn option_int<'a>(opts: &'a [ResolvedOption<'a>], name: &str) -> Option<i64> {
    opts.iter()
        .find(|opt| opt.name == name)
        .and_then(|opt| match opt.value {
            ResolvedValue::Integer(value) => Some(value),
            _ => None,
        })
}

fn option_bool<'a>(opts: &'a [ResolvedOption<'a>], name: &str) -> Option<bool> {
    opts.iter()
        .find(|opt| opt.name == name)
        .and_then(|opt| match opt.value {
            ResolvedValue::Boolean(value) => Some(value),
            _ => None,
        })
}

//! Bot bootstrap: command registration at init, gateway startup, event routing.

use anyhow::Context as _;
use blabber_bot::{commands, config::Config, persistence::Db, player, player::Shared};
use serenity::all::{
    Client, Context, EditInteractionResponse, EventHandler, GatewayIntents, GuildId, Http,
    Interaction, Ready, VoiceState,
};
use songbird::{SerenityInit, Songbird};
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

struct Handler {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Connected as {} — ready to play music", ready.user.name);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            info!(
                command = %command.data.name,
                user = %command.user.name,
                guild = ?command.guild_id,
                "received slash command"
            );
            if let Err(err) = commands::dispatch(&ctx, &self.shared, &command).await {
                error!(command = %command.data.name, %err, "command dispatch failed");
                let _ = command
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(format!("⚠️ {err}")))
                    .await;
            }
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id else {
            return;
        };
        let bot_id = ctx.cache.current_user().id;

        // If the bot itself lost its voice channel (kicked, moved to nothing,
        // server outage), drop the guild's playback state.
        if new.user_id == bot_id && new.channel_id.is_none() {
            info!(%guild_id, "disconnected from voice, clearing player state");
            self.shared.remove_player(guild_id).await;
            self.shared.cancel_empty_timer(guild_id).await;
            let _ = self.shared.songbird.remove(guild_id).await;
            return;
        }

        // Only react to humans moving between channels; bots joining (including
        // this one) and mute/deafen changes don't change who is listening.
        if new.user_id == bot_id {
            return;
        }
        let was_in = old.as_ref().and_then(|state| state.channel_id);
        let is_in = new.channel_id;
        if was_in == is_in {
            return;
        }

        // Nothing playing means there's nothing to time out.
        if self.shared.songbird.get(guild_id).is_none() {
            return;
        }

        if player::occupied_listener_count(&ctx, guild_id) > 0 {
            // Someone is present: cancel any countdown in progress.
            self.shared.cancel_empty_timer(guild_id).await;
            return;
        }

        // The channel just emptied — start the countdown.
        info!(%guild_id, "voice channel is now empty, starting idle countdown");
        self.shared.schedule_empty_disconnect(ctx.clone(), guild_id);
    }
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Arc::new(Config::from_env()?);

    // Command registration runs on its own task, in parallel with the gateway
    // connection, so a slow clear/register cycle can't leave the bot connected
    // but deaf. Deleting and re-adding every command is rate limited to 5 per
    // 20s per guild, which is ~85s for a 21-command set — long enough that
    // awaiting it before connecting made every restart a blackout.
    let http = Http::new(&config.token);
    let app = http
        .get_current_application_info()
        .await
        .context("could not authenticate with Discord — check DISCORD_TOKEN")?;
    http.set_application_id(app.id);
    info!("Authenticated as application {} ({})", app.name, app.id);

    // `Http` is not `Clone`, so move a freshly built one into the task and keep
    // the original for the client builder below.
    let register_http = Http::new(&config.token);
    register_http.set_application_id(app.id);
    let guild_id = config.guild_id.map(GuildId::new);
    tokio::spawn(async move {
        // One bulk PUT replaces the whole set, so this costs a single request
        // instead of one per command.
        if let Err(err) = commands::sync(&register_http, guild_id).await {
            error!(%err, "failed to sync slash commands");
            return;
        }

        // Sweep up leftovers from a previous scope (global commands, or a guild
        // the bot no longer lives in). Cheap and non-fatal: stale commands are
        // cosmetic, so a failure here must not stop the bot from serving.
        match commands::prune_stale(&register_http, guild_id).await {
            Ok(0) => {}
            Ok(removed) => info!("Pruned {removed} stale slash command(s) from other scopes"),
            Err(err) => warn!(%err, "could not prune stale slash commands"),
        }
    });

    let db = Db::open(&config.database_path).context("failed to open the state database")?;
    let songbird = Songbird::serenity();
    let shared = Shared::new(config.clone(), songbird.clone(), db);

    let restored = shared.db.load_all().await.unwrap_or_else(|err| {
        tracing::warn!(%err, "failed to load persisted state");
        Vec::new()
    });
    let restored_count = restored.len();
    shared.restore(restored).await;
    if restored_count > 0 {
        info!("Restored persisted state for {restored_count} guild(s)");
    }

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(config.token.clone(), intents)
        .event_handler(Handler { shared })
        .register_songbird_with(songbird)
        .await
        .context("failed to build the Discord client")?;

    info!("Starting gateway...");
    client.start_autosharded().await.context("gateway connection failed")?;

    Ok(())
}

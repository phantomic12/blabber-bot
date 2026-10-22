//! Bot bootstrap: command registration at init, gateway startup, event routing.

use anyhow::Context as _;
use blabber_bot::{commands, config::Config, persistence::Db, player::Shared};
use serenity::all::{
    Client, Context, EditInteractionResponse, EventHandler, GatewayIntents, GuildId, Http,
    Interaction, Ready, VoiceState,
};
use songbird::{SerenityInit, Songbird};
use std::sync::Arc;
use tracing::{error, info};
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
            if let Err(err) = commands::dispatch(&ctx, &self.shared, &command).await {
                error!(command = %command.data.name, %err, "command dispatch failed");
                let _ = command
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(format!("⚠️ {err}")))
                    .await;
            }
        }
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        // If the bot itself lost its voice channel (kicked, moved to nothing,
        // server outage), drop the guild's playback state.
        if new.channel_id.is_none() {
            if let Some(guild_id) = new.guild_id {
                if new.user_id == ctx.cache.current_user().id {
                    info!(%guild_id, "disconnected from voice, clearing player state");
                    self.shared.remove_player(guild_id).await;
                    let _ = self.shared.songbird.remove(guild_id).await;
                }
            }
        }
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

    // Slash command setup happens at init, before the gateway connects: wipe
    // anything registered previously (global and per guild), then set ours.
    let http = Http::new(&config.token);
    let app = http
        .get_current_application_info()
        .await
        .context("could not authenticate with Discord — check DISCORD_TOKEN")?;
    http.set_application_id(app.id);
    info!("Authenticated as application {} ({})", app.name, app.id);

    info!("Clearing pre-existing slash commands...");
    commands::clear_existing(&http).await?;
    match config.guild_id {
        Some(guild_id) => commands::register_for_guild(&http, GuildId::new(guild_id)).await?,
        None => commands::register(&http).await?,
    }

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

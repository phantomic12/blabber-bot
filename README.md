# blabber-bot

A Discord music bot written in Rust on [serenity] + [songbird], with the usual
music-bot feature set **plus age-restricted video support**.

[serenity]: https://crates.io/crates/serenity
[songbird]: https://crates.io/crates/songbird

## Features

- `/play` for search terms, video/track URLs, and playlists (YouTube, SoundCloud, and anything else yt-dlp supports)
- Queue management: `/queue`, `/remove`, `/clear`, `/shuffle`, `/skipto`
- Playback control: `/pause`, `/resume`, `/skip`, `/stop`, `/seek`, `/volume`
- `/loop` with `off` / `track` / `queue` modes, `/nowplaying`, `/leave`
- **Autoplay**: when the queue runs dry, related tracks keep playing (toggle with `/autoplay`)
- **DJ controls**: DJ role, requester-only mode, and vote-to-skip for busy servers
- **Persistence**: queues and settings survive restarts and redeploys (SQLite)
- **Diagnostics**: `/diagnose` preflights yt-dlp, the JS runtime, and cookie auth, and live-probes an age-restricted URL
- **Age-restricted videos**: works with a cookies session from an age-verified account, with automatic player-client fallbacks
- WebM/Opus streams are passed straight through to Discord without re-encoding
- On startup the bot **deletes every pre-existing slash command** (global and
  guild-scoped, in every guild) and replaces them with its own

## Requirements

- Rust 1.80+
- [yt-dlp] on `PATH` (or set `YTDLP_PATH`)
- A JS runtime for YouTube extraction — [Deno] or Node.js 20+ (recent yt-dlp needs it for YouTube's player challenges)

[yt-dlp]: https://github.com/yt-dlp/yt-dlp
[Deno]: https://deno.com

Windows note: songbird builds libopus from source with CMake. If CMake picks a
Visual Studio install without C++ tools, set `CMAKE_GENERATOR` to one that has
them — [.cargo/config.toml](.cargo/config.toml) already pins
`Visual Studio 17 2022` for this machine.

## Setup

1. Create an application at <https://discord.com/developers/applications>, add
   a bot, and copy the token.
2. Invite it with the `bot` and `applications.commands` scopes plus **Connect**
   and **Speak** permissions.
3. Configure and run:

```bash
cp .env.example .env   # then edit it
cargo run --release
```

At init the bot clears all previously registered application commands and
registers its own. Global command changes can take a few minutes to appear the
first time — set `DISCORD_GUILD_ID` while testing (see below) for instant
commands.

**Keep secrets out of version control.** The bot token (`DISCORD_TOKEN`) and any
`cookies.txt` (which grants access to your YouTube account) must never be
committed — `.gitignore` already excludes `.env` and `cookies.txt`.

## Testing with a scratch token

Global slash commands can take up to an hour to propagate to Discord's clients.
While testing with a scratch bot, set:

```env
DISCORD_GUILD_ID=your_test_server_id
```

Commands are then registered on that guild only and appear instantly. Startup
still clears every pre-existing command (global and guild-scoped) first in both
modes. Drop the variable again to publish commands globally.

Once connected, run `/diagnose` to verify the playback stack (yt-dlp version,
JS runtime, cookie session) and `/diagnose url:<age-restricted link>` to prove
the age-restricted path end to end.

## Age-restricted videos

YouTube only serves age-restricted (and private/members-only) content to a
signed-in, age-verified session. To enable this:

1. Sign into YouTube in a browser with an age-verified account.
2. Export cookies for `youtube.com` with a browser extension such as
   *Get cookies.txt LOCALLY*, saving a Netscape-format `cookies.txt`.
3. Point the bot at it with a plain Unix path — relative paths resolve from the
   working directory the bot runs in, so dropping `cookies.txt` next to the bot
   and using the default `./cookies.txt` is enough:

```env
YTDLP_COOKIES=./cookies.txt
```

Absolute paths (`/home/bot/cookies.txt`) and `~/cookies.txt` work too. In
Docker the file lives inside the container, so use the mount path shown in
[Docker](#docker) below (`/home/bot/cookies.txt`).

or let yt-dlp read the browser profile directly:

```env
YTDLP_COOKIES_FROM_BROWSER=firefox
```

The extractor always tries the configured cookie session first, then falls back
to the `tv`/`android` player clients (which occasionally bypass the age gate
without a session). If extraction fails you'll get the actual yt-dlp error plus
a hint about these settings. Keep `cookies.txt` out of version control (it is
already in [.gitignore](.gitignore)) — anyone with it can use your account.

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `DISCORD_TOKEN` | — | Bot token (required) |
| `YTDLP_PATH` | `yt-dlp` | yt-dlp executable |
| `YTDLP_COOKIES` | — | Netscape cookies.txt for age-restricted/private videos |
| `YTDLP_COOKIES_SOURCE` | — | Docker only: read-only export the entrypoint copies to `YTDLP_COOKIES` |
| `YTDLP_COOKIES_FROM_BROWSER` | — | Browser to read cookies from (`firefox`, `chrome`, …) |
| `YTDLP_EXTRA_ARGS` | — | Extra yt-dlp arguments, space-separated |
| `MAX_PLAYLIST` | `25` | Max playlist entries per request (1–100) |
| `DEFAULT_VOLUME` | `100` | Startup volume, 0–200% |
| `DATABASE_PATH` | `blabber-bot.db` | SQLite database for queues + settings |
| `AUTOPLAY` | `true` | Autoplay related tracks when the queue runs dry |
| `EMPTY_CHANNEL_TIMEOUT` | `300` | Seconds of empty voice channel before stopping and disconnecting; `0` disables |
| `DISCORD_GUILD_ID` | — | Register commands on one guild only (instant, for testing) |
| `RUST_LOG` | `info` | Log level |

## Docker

The [Dockerfile](Dockerfile) is multi-stage: a Rust builder (opus is statically
linked, so the runtime carries no audio libraries) and a slim runtime shipping
**yt-dlp + Python + Node** — the JS runtime yt-dlp needs for YouTube. ffmpeg is
not needed.

```bash
cp .env.example .env    # fill in DISCORD_TOKEN
docker compose up -d --build
docker compose logs -f
```

Age-restricted playback works the same as bare-metal: export `cookies.txt`,
uncomment the cookies mount in [docker-compose.yml](docker-compose.yml), and
set `YTDLP_COOKIES_SOURCE=/home/bot/cookies.source` in `.env`.

The mount is read-only on purpose. yt-dlp rewrites its cookie jar on every
extraction to persist rotated session cookies, and that rewrite replaces the
file with yt-dlp's in-memory copy — which drops the login cookies
(`SID`, `HSID`, `SSID`, `APISID`, `SAPISID`, `LOGIN_INFO`). Pointed straight at
your export, a single lookup could silently invalidate it. The entrypoint copies
the source to a writable jar under the `bot-data` volume and points
`YTDLP_COOKIES` there, so your export stays pristine.

Notes:

- The container runs as a non-root user; compose persists yt-dlp's cache in the
  `yt-dlp-cache` volume so re-resolving streams is fast.
- Queues and settings persist in the `bot-data` volume (`DATABASE_PATH` is set
  automatically), so redeploys don't lose the playlist.
- When running without compose, use `docker run --init` so signals are
  forwarded and yt-dlp child processes get reaped.
- Rebuild the image when YouTube extraction breaks — that usually means yt-dlp
  needs updating: `docker compose build --no-cache && docker compose up -d`.

## Commands

| Command | Description |
| --- | --- |
| `/play query:` | Play a search term, URL, or playlist |
| `/skip` · `/skipto position:` | Skip the current track / jump to a queue position |
| `/pause` · `/resume` · `/stop` | Pause, resume, or stop and clear |
| `/seek position:` | Seek to `90`, `1:30`, or `1:02:03` |
| `/volume level:` | Set volume, 0–200% |
| `/queue page:` | Show the queue |
| `/nowplaying` | Current track with progress bar |
| `/shuffle` | Shuffle the queue |
| `/loop mode:` | `off` / `track` / `queue` |
| `/remove position:` · `/clear` | Remove one track / clear the queue |
| `/autoplay enabled:` | Toggle autoplay of related tracks |
| `/djrole role:` | Set/clear the DJ role (restricts control commands) |
| `/requestersonly enabled:` | Only the requester (and DJs) control a track |
| `/voteskip enabled: threshold:` | Let non-DJs vote to skip |
| `/settings` | Show this server's playback settings |
| `/diagnose url:` | Preflight checks + live extraction probe |
| `/leave` | Disconnect |
| `/refreshcommands` | Re-sync slash commands with Discord (Manage Server) |

## Behaviour notes

**Idle disconnect.** When everyone leaves the voice channel, the bot keeps
playing for `EMPTY_CHANNEL_TIMEOUT` seconds (default 5 minutes) and then stops,
clears the queue, and disconnects. The grace period exists so a brief
disconnect-and-rejoin — a client update, a dropped connection, walking out of
range — doesn't kill the session. If someone returns before it expires, the
countdown is cancelled silently. Set `EMPTY_CHANNEL_TIMEOUT=0` to stay connected
indefinitely.

**Slash commands.** Registration is a single bulk `PUT` per boot, which replaces
the whole command set atomically. `/refreshcommands` re-runs it at runtime, so
adding or renaming a command no longer needs a restart. Discord also caches
commands client-side; `Ctrl+R` clears that if they look stale.

## Development

```bash
cargo test
cargo clippy --all-targets
```

The extractor integration tests compile a fake `yt-dlp` with `rustc` at test
time (see [tests/extractor_integration.rs](tests/extractor_integration.rs)), so
they run offline and cover the age-gate strategy chain end to end. CI
([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs tests + clippy on
Linux and Windows, then builds the Docker image.

Queue state is per guild and persisted to SQLite (`DATABASE_PATH`); the
interrupted track goes first in line when restored, and `/leave` clears a
guild's stored state.

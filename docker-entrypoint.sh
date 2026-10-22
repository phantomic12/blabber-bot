#!/bin/sh
# Copy the cookie jar to a writable location before yt-dlp touches it.
#
# yt-dlp rewrites the cookie file on exit to persist rotated session cookies. If
# YTDLP_COOKIES points at the bind-mounted export, that rewrite replaces the
# whole jar with yt-dlp's own in-memory copy, which drops the login cookies
# (SID/HSID/SSID/APISID/SAPISID/LOGIN_INFO) it declined to carry over - the
# source export is destroyed and age-restricted playback breaks until the user
# exports again. Mounting read-only instead makes every extraction warn and
# still fail to persist the rotation.
#
# So: keep the export pristine under /home/bot/cookies.source, and point yt-dlp
# at a writable copy inside the data volume that can be rewritten freely.
set -eu

if [ -n "${YTDLP_COOKIES_SOURCE:-}" ] && [ -f "$YTDLP_COOKIES_SOURCE" ]; then
    target="${YTDLP_COOKIES:-/home/bot/data/cookies.txt}"
    if [ ! -f "$target" ] || [ "$YTDLP_COOKIES_SOURCE" -nt "$target" ]; then
        cp "$YTDLP_COOKIES_SOURCE" "$target"
        chmod 600 "$target"
    fi
    export YTDLP_COOKIES="$target"
fi

exec /usr/local/bin/blabber-bot "$@"

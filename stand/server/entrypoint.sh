#!/bin/bash
# starts the local test server; compose mounts the mod list at /config/mod-list.json
set -euo pipefail

factorio=/opt/factorio/bin/x64/factorio
mods=/data/mods
save=/data/saves/probe.zip
mkdir -p "$mods" /data/saves
cp /config/mod-list.json "$mods/mod-list.json"

if [ ! -f "$save" ]; then
  "$factorio" --create "$save" --mod-directory "$mods"
fi

args=(--start-server "$save" --server-settings /config/server-settings.json --mod-directory "$mods" --port 34197)
# an empty value disables lua udp for negative tests
if [ -n "${LUA_UDP_PORT:-}" ]; then
  args+=(--enable-lua-udp "$LUA_UDP_PORT")
fi
exec "$factorio" "${args[@]}"

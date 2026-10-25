# Hub

When you finish this page, the relay hub runs beside your Factorio server, the players' companions can use it, and you know how to read the summary it prints when it stops.

## What the hub is for

The companion measures the delay of several routes to each other player and switches between them on its own:

- **direct**: straight between the two companions;
- **through a player relay**: through a third player's companion;
- **through the hub**, if the server runs one;
- **through the game**: always there, and the fallback when no other route is live.

The companion uses the live route with the lowest measured delay; the hub is one more route, measured like the others, not a fast lane. When the first network route comes up, the companion moves to it at once; if the route in use dies, it moves at once to the fastest live one. Otherwise it changes routes only when another has been at least 10 ms faster for 3 seconds, and that includes going back to the game route. So the game route is usually the slowest, but not always.

When two players cannot reach each other directly, typically because one sits behind a strict NAT, the hub gives them another route, through the server machine. The hub is optional: without it voice still gets through, by the other routes. The route in use shows in the tooltip of the link-quality bars at the right end of a speaker's row in the speaker stack (not on the row or the name), for example *Link: through the hub, delay unknown* (the companion does not report the delay yet, so it always reads *delay unknown*). A row with the red echo mark has no bars, so it shows no route.

The hub plays no audio and needs no sound card. To the mod it presents itself as player 0, over the server's Lua UDP port on loopback. That decides how you run it:

- the hub must run **on the same machine as the server**, in the same network namespace;
- the server must start with `--enable-lua-udp <port>` (see [Server Setup](Server-Setup));
- only the hub's public port is opened to the players.

## Ports

| Port | Who uses it | Open in the firewall? |
| --- | --- | --- |
| `34197/udp` | the Factorio game port | yes, as usual |
| `34560/udp` | the hub's public port (`--public`), companions connect here | yes, and forward it through NAT (UDP, not TCP) |
| `34556/udp` | the server's Lua UDP port (`--enable-lua-udp`, the hub's `--game`) | no, loopback only |
| `34557/udp` | where the hub hears the mod (`--receive`) | no, loopback only |

## Run it with Docker

The image is `ghcr.io/vshuraeff/ptt-radio-hub` for `linux/amd64` and `linux/arm64`, tagged with the version (`0.4.0`), the minor line (`0.4`) and `latest`. Arguments after the image name go straight to the hub.

**The server runs directly on the host.** Use the host network, so `127.0.0.1` inside the container is the host's loopback:

1. Start the Factorio server with `--enable-lua-udp 34556`.
2. Start the hub:

   ```sh
   docker run -d --name ptt-radio-hub --network host --restart unless-stopped \
     ghcr.io/vshuraeff/ptt-radio-hub:0.4.0 \
     --public 0.0.0.0:34560 --game 127.0.0.1:34556 --receive 127.0.0.1:34557
   ```

3. Check the log: `docker logs ptt-radio-hub` shows `hub: public 0.0.0.0:34560 | game 127.0.0.1:34556 via 127.0.0.1:34557`.

**The server runs in a container.** The hub joins the server container's network namespace. A container in a shared namespace cannot publish ports itself, so the hub's port `34560/udp` is published on the server's container. With Docker Compose:

```yaml
services:
  factorio:
    # your Factorio server image, started with --enable-lua-udp 34556
    ports:
      - "34197:34197/udp"
      - "34560:34560/udp"   # the hub's public port
  hub:
    image: ghcr.io/vshuraeff/ptt-radio-hub:0.4.0
    network_mode: "service:factorio"
    depends_on:
      - factorio
    restart: unless-stopped
    command: ["--public", "0.0.0.0:34560", "--game", "127.0.0.1:34556", "--receive", "127.0.0.1:34557"]
```

The repository has a complete example in [`docker/compose.example.yaml`](https://github.com/vshuraeff/ptt-radio/blob/master/docker/compose.example.yaml). With plain `docker run`, the same is `--network container:<server container name>`.

After the server container is recreated or restarted, restart the hub too: it stays in the old container's namespace.

The image has no shell, so `docker exec ... sh` does not work; read the hub with `docker logs`.

## Run it from the release binary

1. Download `ptt-radio-hub-<version>-linux-x86_64.tar.gz` (or `-linux-aarch64` for ARM) and `SHA256SUMS` from the [Releases page](https://github.com/vshuraeff/ptt-radio/releases).
2. Check and unpack:

   ```sh
   sha256sum --check --ignore-missing SHA256SUMS
   tar -xzf ptt-radio-hub-0.4.0-linux-x86_64.tar.gz
   sudo install -m 755 ptt-radio-hub /usr/local/bin/ptt-radio-hub
   ```

   The binary is static and needs no libraries.

3. Create `/etc/systemd/system/ptt-radio-hub.service`:

   ```ini
   [Unit]
   Description=ptt-radio relay hub
   After=network-online.target
   Wants=network-online.target

   [Service]
   ExecStart=/usr/local/bin/ptt-radio-hub --public 0.0.0.0:34560 --game 127.0.0.1:34556 --receive 127.0.0.1:34557
   DynamicUser=yes
   Restart=on-failure

   [Install]
   WantedBy=multi-user.target
   ```

4. Start it: `sudo systemctl enable --now ptt-radio-hub`, and read the log with `journalctl -u ptt-radio-hub`.

## Options

| Option | Default | What it does |
| --- | --- | --- |
| `--public <IP:PORT>` | `0.0.0.0:34560` | The UDP address companions connect to and relay through. |
| `--game <IP:PORT>` | `127.0.0.1:34556` | The server's Lua UDP address: the port after the server's `--enable-lua-udp`. |
| `--receive <IP:PORT>` | `127.0.0.1:34557` | The local UDP address where the hub hears the mod. The hub tells the mod only the port, every 5 seconds, and the mod sends to that port on the server's own localhost, so the IP must be `127.0.0.1` or a wildcard such as `0.0.0.0`. The port must be 1024 or higher: the mod ignores a lower one without any error. |
| `--grant-ttl-s <SECONDS>` | `1800` | How long one relay permission between two players is valid. When less than a quarter of that time is left and both players have used the permission within the last quarter, the hub issues a fresh one, so a pair that keeps using the hub route (route probes count too) keeps it. Otherwise the permission expires, and the hub offers the pair a fresh one at the next announce from either companion, about 5 seconds later, so two players keep a hub route as long as both companions run with network routes. |
| `--player-grants <N>` | `64` | How many live relay permissions one player may hold. |
| `--player-rate <BYTES/S>` | `65536` | How many bytes per second the hub sends on behalf of one player, with a burst of up to one second's worth. A datagram over the limit is dropped and counted as `RateLimited` under `dropped:`. |

All addresses are IP addresses with a port. `localhost` or a host name is not accepted.

## Tell the players' companions

Each player starts the companion with the hub's address: the same server address they connect to, and the public port:

```sh
ptt-radio-companion --hub 203.0.113.10:34560
```

This too takes an IP address, not a host name. Players on the same local network as the server should use the server's local address: many routers cannot reach their own public address from inside.

Players who start the game through the companion's `--launch` put `--hub` before `--launch`: everything after `--launch` is the game's command line, so a `--hub` placed after it goes to the game, and the companion runs without the hub. The Steam launch option the installer prints has no `--hub`; add it like this:

```sh
"<install folder>/ptt-radio-companion" --hub 203.0.113.10:34560 --launch %command%
```

Without `--hub` the companion does not use the hub: it keeps the direct route, the player relay and the game. With `--game-only` it opens no network route of its own (no direct route, no relays, no hub) and sends voice through the game only.

To see which route a companion picked, start it with `--trace <file>`. The file gets a line each time the route to a player changes, starting with a Unix timestamp in microseconds:

```text
<time> switch <player> Direct selected
<time> switch <player> Hub { session: N } selected
<time> switch <player> PlayerRelay { player: N, session: N } selected
<time> switch <player> Game selected
```

The companion writes the file through a buffer: lines arrive in chunks, and the rest when the companion exits normally, `Ctrl-C` included. Read the file after the companion has stopped; while it runs, the file lags behind and may show no `switch` lines yet.

Each copy of a voice frame sent over the network is a `route_tx` line. One copy always goes through the game as well, so game traffic alone does not mean the direct route failed.

## Is it working?

The hub has no live counters and prints no line when the mod registers it. At start it prints only its own addresses (`hub: public ... | game ... via ...`). It prints a summary line when it stops (`docker stop`, `systemctl stop` or `Ctrl-C`):

```text
hub: forwarded N | bind replies N | signals N | refused grants N | dropped: ...
```

- `signals`: messages the hub sent to the game, almost all of them relay permissions for a pair of players, with their renewals and closes. Above zero means the hub got the companions' announcements through the mod and sent permissions back; it does not prove the mod received them.
- `bind replies`: the challenges and acknowledgements the hub sent to companions registering on the public port. One registration normally takes two, and a repeated request gets one more; requests the hub drops are not counted. Above zero means companions reach the public port with valid permissions. It is not a count of companions or of attempts.
- `forwarded`: packets passed between players.
- `refused grants`: failed attempts to issue or renew a relay permission. A new permission is refused when either player already holds `--player-grants` live ones, when the hub's table is full (4096 permissions), or when the system's random source fails. A renewal is refused only when the table is full, when the pair already has a replacement, or when the random source fails; the hub then retries about once a second until 30 seconds after the old permission expires, and each failed retry counts again. Going over `--player-rate` never changes this counter.
- `dropped:`: datagrams the hub discarded, as `<reason> <count>` pairs separated by commas, or `none`. The reasons are the relay's names for them, such as `UnknownSession`, `Expired`, `RateLimited` or `BadProof`, plus the error kind of any socket receive error, for example `ConnectionReset`.

`signals 0` is normal while fewer than two players run a companion without `--game-only`, even when the hub is healthy: the hub hands out permissions only to a pair of players whose companions have both announced themselves with network routes. A companion announces itself to the hub through the mod once at least one other player is connected, then every 5 seconds. A companion with `--game-only` announces itself too, but without network routes, and gets nothing from the hub.

## If it does not work

- **`signals 0` with two or more players online whose companions run without `--game-only`.** The hub and the mod do not reach each other. Check that the hub's `--game` port matches the server's `--enable-lua-udp`, that the server really runs with that flag, that the hub shares the server's network (`--network host`, or `network_mode: "service:factorio"`), and that `--receive` uses `127.0.0.1` or `0.0.0.0` with a port of 1024 or higher. The mod forgets the hub after 15 seconds without word from it; after the server container restarts, restart the hub too, because it stays in the old container's namespace.
- **`error: cannot bind public ...` or `error: cannot bind receive ...`.** The port is already taken on the server, for example by a second hub. Stop the other process or change `--public` or `--receive`; the mod learns a new receive port from the hub on its own.
- **`bind replies 0` while `signals` is above zero.** The companions do not reach the public port: it is not published (with Compose, check `docker compose ps` for the `factorio` service), blocked by the firewall, or not forwarded through NAT.
- **Players on the same LAN as the server.** Many routers cannot reach their own public address from inside (no hairpin NAT). The hub listens on `0.0.0.0`, so players on the LAN can use the server's local address in `--hub`.
- **The hub was restarted during a game.** It starts a new session, and every old relay permission stops working. The companions announce themselves again and get new ones.

## Building the image yourself

The published image is built from [`docker/hub.Dockerfile`](https://github.com/vshuraeff/ptt-radio/blob/master/docker/hub.Dockerfile) in the repository. To build only the hub binary, see [Building from Source](Building-from-Source#build-only-the-hub).

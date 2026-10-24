# Hub

When you finish this page, the relay hub runs beside your Factorio server, the players' companions use it, and you can tell from its log that it registered with the game.

## What the hub is for

The companion measures several routes to each other player and switches between them on its own:

- **direct**: straight between the two companions;
- **through a player relay**: through a third player's companion;
- **through the hub**, if the server runs one;
- **through the game**: always available, but slower.

When two players cannot reach each other directly, typically because one sits behind a strict NAT, the hub gives them a fast route through the server machine. The hub is optional: without it voice still gets through, by the other routes. The hover text on a speaker in the speaker stack shows the route in use, for example *through the hub*.

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
| `34557/udp` | where the mod answers the hub (`--receive`) | no, loopback only |

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
| `--receive <IP:PORT>` | `127.0.0.1:34557` | The local address where the mod answers the hub. The hub tells the mod this port itself. |
| `--grant-ttl-s <SECONDS>` | `1800` | How long one relay permission between two players lasts. |
| `--player-grants <N>` | `64` | How many live relay permissions one player may hold. |
| `--player-rate <BYTES/S>` | `65536` | How many bytes per second the hub sends on behalf of one player. |

All addresses are IP addresses with a port. `localhost` or a host name is not accepted.

## Tell the players' companions

Each player starts the companion with the hub's address: the same server address they connect to, and the public port:

```sh
ptt-radio-companion --hub 203.0.113.10:34560
```

This too takes an IP address, not a host name. Players on the same local network as the server should use the server's local address: many routers cannot reach their own public address from inside.

Without `--hub` the companion does not use the hub: it keeps the direct route, the player relay and the game. With `--game-only` it opens no network routes at all and sends voice through the game only.

To see which route a companion picked, start it with `--trace <file>`. The file gets a line each time the route to a player changes:

```text
switch <player> Direct selected
switch <player> Hub { session: N } selected
switch <player> PlayerRelay { ... } selected
switch <player> Game selected
```

Each copy of a voice frame sent over the network is a `route_tx` line. One copy always goes through the game as well, so game traffic alone does not mean the direct route failed.

## Is it working?

The hub has no live counters. It prints a summary line when it stops (`docker stop`, `systemctl stop` or `Ctrl-C`):

```text
hub: forwarded N | bind replies N | signals N | refused grants N | dropped: ...
```

- `signals` above zero: the mod talked to the hub and the hub handed out relay permissions, so registration worked.
- `bind replies`: companions that reached the public port.
- `forwarded`: packets passed between players.
- `refused grants`: permissions refused because they hit the hub's limits.

`signals 0` is normal while no player has started a companion with network routes: a companion with `--game-only` does not ask the hub for anything.

## If it does not work

- **`signals 0` with players and their companions online.** The hub's messages do not reach the mod. Check that the hub's `--game` port matches the server's `--enable-lua-udp`, that the server really runs with that flag, and that the hub shares the server's network (`--network host`, or `network_mode: "service:factorio"`). The mod forgets the hub after 15 seconds without word from it; after the server container restarts, restart the hub too, because it stays in the old container's namespace.
- **`error: cannot bind public ...` or `error: cannot bind receive ...`.** The port is already taken on the server, for example by a second hub. Stop the other process or change `--public` or `--receive`; the mod learns a new receive port from the hub on its own.
- **`bind replies 0` while `signals` is above zero.** The companions do not reach the public port: it is not published (with Compose, check `docker compose ps` for the `factorio` service), blocked by the firewall, or not forwarded through NAT.
- **Players on the same LAN as the server.** Many routers cannot reach their own public address from inside (no hairpin NAT). The hub listens on `0.0.0.0`, so players on the LAN can use the server's local address in `--hub`.
- **The hub was restarted during a game.** It starts a new session, and every old relay permission stops working. The companions announce themselves again and get new ones.

## Building the image yourself

The published image is built from [`docker/hub.Dockerfile`](https://github.com/vshuraeff/ptt-radio/blob/master/docker/hub.Dockerfile) in the repository. To build only the hub binary, see [Building from Source](Building-from-Source#build-only-the-hub).

# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fake-voice companion for testing the ptt-radio mod without audio.

Packets to the game (127.0.0.1:<--game-port>, the --enable-lua-udp value):
  R, 1, kind, seq u16 BE, body          kind: 1 HELLO, 2 START, 3 VOICE, 4 STOP
Packets from the game (127.0.0.1:<--port>, the ptt-radio-companion-port setting):
  R, 1, kind, sender u16 BE, rest       kind: 2/3/4 forwarded, 0x10 ROSTER, 0x11 MIC_STATE (rest: 1 byte, 0 off / 1 on)

Commands:
  hello   send one HELLO, print the roster and open-mic state the mod answers with
  talk    hold the radio for N seconds sending fake 20 ms frames, then release
  listen  print what the mod forwards to this companion; --heartbeat keeps the
          mod's companion presence alive by sending HELLO every 5 s
"""

import argparse
import socket
import struct
import time

HELLO, START, VOICE, STOP, ROSTER, MIC_STATE = 0x01, 0x02, 0x03, 0x04, 0x10, 0x11
NAMES = {START: "START", VOICE: "VOICE", STOP: "STOP", ROSTER: "ROSTER", MIC_STATE: "MIC_STATE"}
HEARTBEAT_SECONDS = 5


def packet(kind: int, seq: int, body: bytes = b"") -> bytes:
    return struct.pack(">BBBH", 0x52, 1, kind, seq) + body


def parse(data: bytes):
    if len(data) < 5 or data[0] != 0x52 or data[1] != 1:
        return None
    kind = data[2]
    sender = struct.unpack(">H", data[3:5])[0]
    return kind, sender, data[5:]


def open_socket(port: int) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", port))
    sock.setblocking(False)
    return sock


def receive(sock: socket.socket):
    while True:
        try:
            data, _ = sock.recvfrom(70000)
        except BlockingIOError:
            return
        parsed = parse(data)
        if parsed:
            yield time.monotonic(), parsed


def print_roster(body: bytes):
    for line in body.decode(errors="replace").splitlines():
        index, name, color = line.split("\t")
        print(f"  player {index:>3}  {name:<24} rgb({color})")


def cmd_hello(args):
    sock = open_socket(args.port)
    sock.sendto(packet(HELLO, 0, b"radio.py"), ("127.0.0.1", args.game_port))
    deadline = time.monotonic() + 3
    roster = mic = None
    while time.monotonic() < deadline and (roster is None or mic is None):
        for _, (kind, _, body) in receive(sock):
            if kind == ROSTER:
                roster = body
            elif kind == MIC_STATE:
                mic = body[:1] == b"\x01"
        time.sleep(0.01)
    if roster is None:
        print("no ROSTER within 3 s: game not running with --enable-lua-udp, wrong port, or mod not loaded")
        return
    print("roster:")
    print_roster(roster)
    print("open mic: " + ("no MIC_STATE (mod older than 0.2)" if mic is None else "on" if mic else "off"))


def cmd_talk(args):
    sock = open_socket(args.port)
    target = ("127.0.0.1", args.game_port)
    frames = int(args.seconds * 50)
    events = []
    start = time.monotonic()
    sent_at = {}
    sock.sendto(packet(START, 0), target)
    for seq in range(1, frames + 1):
        due = start + seq * 0.02
        while (delay := due - time.monotonic()) > 0:
            events.extend(receive(sock))
            time.sleep(min(delay, 0.002))
        sent_at[seq] = time.monotonic()
        sock.sendto(packet(VOICE, seq, bytes((seq + i) % 256 for i in range(args.size))), target)
    released = time.monotonic()
    for _ in range(0 if args.no_stop else 2):
        sock.sendto(packet(STOP, frames), target)
    deadline = released + args.grace
    while time.monotonic() < deadline:
        events.extend(receive(sock))
        time.sleep(0.005)

    voice = [(t, struct.unpack(">H", body[:2])[0]) for t, (kind, _, body) in events if kind == VOICE]
    kinds = {}
    for _, (kind, sender, _) in events:
        kinds.setdefault((NAMES.get(kind, hex(kind)), sender), 0)
        kinds[(NAMES.get(kind, hex(kind)), sender)] += 1
    print(f"sent START + {frames} VOICE + {0 if args.no_stop else 2} STOP over {args.seconds} s")
    print("received (kind, sender): " + (", ".join(f"{k}@{s}={n}" for (k, s), n in sorted(kinds.items())) or "nothing"))
    if voice:
        seqs = [s for _, s in voice]
        in_order = all(b == a + 1 for a, b in zip(seqs, seqs[1:]))
        first_ms = (voice[0][0] - (start + 0.02)) * 1000
        stops = [t for t, (kind, _, _) in events if kind == STOP]
        tail_ms = (stops[0] - released) * 1000 if stops else None
        print(f"echoed voice frames: {len(seqs)}/{frames}, contiguous: {in_order}, first frame after {first_ms:.0f} ms")
        rtts = [(t - sent_at[s]) * 1000 for t, s in voice if s in sent_at]
        ordered = sorted(rtts)
        pick = lambda p: ordered[min(len(ordered) - 1, int(p / 100 * (len(ordered) - 1)))]
        print(
            f"frame rtt ms: p50 {pick(50):.0f}, p95 {pick(95):.0f}, max {ordered[-1]:.0f}; "
            f"first 10: {', '.join(f'{r:.0f}' for r in rtts[:10])}"
        )
        if tail_ms is not None:
            print(f"release -> STOP echoed after {tail_ms:.0f} ms")
    elif not args.expect_silence:
        print("no voice echoed: enable 'Echo my own voice' in mod settings (per player) to hear yourself")


def cmd_listen(args):
    sock = open_socket(args.port)
    deadline = time.monotonic() + args.duration
    next_hello = time.monotonic() if args.heartbeat else deadline
    talk = {}  # sender -> [first packet time, voice frames, last voice frame time]
    mic = None
    while time.monotonic() < deadline:
        if time.monotonic() >= next_hello:
            sock.sendto(packet(HELLO, 0, b"radio.py"), ("127.0.0.1", args.game_port))
            next_hello += HEARTBEAT_SECONDS
        for t, (kind, sender, body) in receive(sock):
            if kind == ROSTER:
                if not args.heartbeat:
                    print(f"{time.strftime('%H:%M:%S')} ROSTER")
                    print_roster(body)
            elif kind == MIC_STATE:
                state = body[:1] == b"\x01"
                if state != mic:
                    print(f"{time.strftime('%H:%M:%S')} open mic {'on' if state else 'off'} for player {sender}")
                    mic = state
            elif kind == START:
                talk[sender] = [t, 0, t]
                print(f"{time.strftime('%H:%M:%S')} player {sender} pressed the radio")
            elif kind == VOICE:
                state = talk.setdefault(sender, [t, 0, t])
                state[1] += 1
                state[2] = t
            elif kind == STOP and sender in talk:
                begun, count, last = talk.pop(sender)
                print(
                    f"{time.strftime('%H:%M:%S')} player {sender} released after {t - begun:.2f} s, "
                    f"{count} frames, STOP {(t - last) * 1000:.0f} ms after the last frame"
                )
        time.sleep(0.005)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--game-port", type=int, default=34556)
    parser.add_argument("--port", type=int, default=34555)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("hello").set_defaults(func=cmd_hello)
    talk = sub.add_parser("talk")
    talk.add_argument("--seconds", type=float, default=2)
    talk.add_argument("--size", type=int, default=40, help="fake opus frame bytes (16 kbit/s, 20 ms = 40)")
    talk.add_argument("--grace", type=float, default=1.5)
    talk.add_argument("--expect-silence", action="store_true")
    talk.add_argument("--no-stop", action="store_true", help="simulate a companion that dies mid-transmission")
    talk.set_defaults(func=cmd_talk)
    listen = sub.add_parser("listen")
    listen.add_argument("--duration", type=float, default=60)
    listen.add_argument("--heartbeat", action="store_true", help="act as a live companion: HELLO every 5 s")
    listen.set_defaults(func=cmd_listen)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

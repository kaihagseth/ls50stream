#!/usr/bin/env python3
"""Latency experiment: how small can the speaker's buffer be kept?

Streams an audio file as live, real-time-paced 24-bit WAV, with:
  --burst S   send S seconds of silence instantly at the start, so the speaker
              fills its start-up buffer and begins playing right away
  --drain S   then send nothing for S seconds, letting the speaker play its
              buffer down, before continuing in real time (skipping that much
              of the source, as a live source would)
and measures the delay = audio sent - position the speaker reports playing
(port 10100 `timep`, sampled at each whole-second change), plus the speaker's
buffer level (`buflvl`) and any position stalls.

  python3 -I latency_test.py <file> [--rate 96000] [--burst 1.9] [--drain 1.0] [--seconds 30]
"""
import argparse
import re
import socket
import subprocess
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

p = argparse.ArgumentParser()
p.add_argument("file")
p.add_argument("--renderer", default="192.168.50.100")
p.add_argument("--me", default=None, help="our IP (default: the route to the renderer)")
p.add_argument("--port", type=int, default=8303)
p.add_argument("--rate", type=int, default=96000)
p.add_argument("--burst", type=float, default=0.0)
p.add_argument("--drain", type=float, default=0.0)
p.add_argument("--seconds", type=float, default=30)
p.add_argument("--bits", type=int, choices=[16, 24, 32], default=24)
p.add_argument("--data-secs", type=float, default=None, help="claim this many seconds of data in the WAV header (default: max)")
args = p.parse_args()

FRAME = 2 * args.bits // 8
BPS = args.rate * FRAME
if args.me is None:
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    probe.connect((args.renderer, 9))
    args.me = probe.getsockname()[0]
URL = f"http://{args.me}:{args.port}/live.wav"
T0 = time.time()

def log(*a):
    print(f"+{time.time() - T0:6.2f}s", *a, flush=True)

# Decode the source once; it is sent as if captured live.
source = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", args.file, "-t", str(args.seconds + 10),
                         "-ar", str(args.rate), "-ac", "2", "-acodec", f"pcm_s{args.bits}le", "-f", f"s{args.bits}le", "pipe:1"],
                        capture_output=True, check=True).stdout

def wav_header(rate):
    import struct
    return (b"RIFF" + struct.pack("<I", 0xFFFFFFFF) + b"WAVEfmt " + struct.pack("<IHHIIHHHHI", 40, 0xFFFE, 2, rate, rate * FRAME, FRAME, args.bits, 22, args.bits, 3)
            + bytes([0x01, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]) + b"data" + struct.pack("<I", DATA_SIZE))

DATA_SIZE = 0xFFFFFFFF if args.data_secs is None else int(args.data_secs * args.rate) * FRAME

state = {"sent_stream_s": 0.0, "start": None}

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        log("GET from speaker")
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("transferMode.dlna.org", "Streaming")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(wav_header(args.rate))
        start = time.time()
        state["start"] = start
        sent = 0  # bytes of stream (including burst silence)
        try:
            burst = int(args.burst * args.rate) * FRAME
            if burst:
                self.wfile.write(bytes(burst))
                sent += burst
            live_from = start + args.drain  # live audio starts after the drain pause
            pos = 0  # source position (bytes)
            while time.time() - start < args.seconds:
                now = time.time()
                if now < live_from:
                    time.sleep(0.005)
                    continue
                # Real time: the source has produced (now - live_from) seconds since live start
                due = int((now - live_from) * args.rate) * FRAME
                if due > pos:
                    chunk = source[pos:due]
                    self.wfile.write(chunk)
                    sent += len(chunk)
                    pos = due
                state["sent_stream_s"] = sent / BPS
                time.sleep(0.01)
        except (BrokenPipeError, ConnectionResetError):
            log("speaker closed the stream")

def soap(action, body):
    env = (f'<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
           f's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">'
           f'<InstanceID>0</InstanceID>{body}</u:{action}></s:Body></s:Envelope>')
    req = urllib.request.Request(f"http://{args.renderer}:8080/AVTransport/ctrl", data=env.encode(),
                                 headers={"Content-Type": 'text/xml; charset="utf-8"',
                                          "SOAPACTION": f'"urn:schemas-upnp-org:service:AVTransport:1#{action}"'})
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.status

results = {"lags": [], "buflvl": [], "positions": []}

def watch():
    s = socket.create_connection((args.renderer, 10100), timeout=3)
    s.settimeout(0.05)
    buf, last_pos = "", None
    while True:
        try:
            buf += s.recv(65536).decode("utf-8", "replace")
        except socket.timeout:
            continue
        while (m := re.search(r"<(view|update) id=[^>]*>.*?</\1>", buf, re.S)):
            frag, buf = m.group(0), buf[m.end():]
            if 'id="play"' not in frag[:30]:
                continue
            if (b := re.search(r'buflvl[^>]*>(\d+)<', frag)):
                results["buflvl"].append(int(b.group(1)))
            if (t := re.search(r'timep[^>]*>(\d+)<', frag)):
                pos = int(t.group(1))
                if pos != last_pos and state["start"]:
                    last_pos = pos
                    results["positions"].append((time.time(), pos))
                    if pos > 0:
                        results["lags"].append(state["sent_stream_s"] - pos)

server = ThreadingHTTPServer((args.me, args.port), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Thread(target=watch, daemon=True).start()

log(f"rate {args.rate}, burst {args.burst}s, drain {args.drain}s -> {URL}")
meta = (f'&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" '
        f'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"&gt;&lt;item id="t" parentID="0" restricted="1"&gt;'
        f'&lt;dc:title&gt;latency test&lt;/dc:title&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;'
        f'&lt;res protocolInfo="http-get:*:audio/wav:DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"&gt;{URL}&lt;/res&gt;'
        f'&lt;/item&gt;&lt;/DIDL-Lite&gt;')
soap("SetAVTransportURI", f"<CurrentURI>{URL}</CurrentURI><CurrentURIMetaData>{meta}</CurrentURIMetaData>")
soap("Play", "<Speed>1</Speed>")
time.sleep(args.seconds + 1)
soap("Stop", "")

pos = results["positions"]
first_play = next((t for t, p in pos if p >= 1), None)
stalls = sum(1 for (t1, p1), (t2, p2) in zip(pos, pos[1:]) if t2 - t1 > 1.5)
lags = results["lags"][3:]  # settle first
if state["start"] and first_play:
    print(f"positions seen: {[p for _, p in pos][-3:]}, last at +{pos[-1][0] - state['start']:.1f}s")
    print(f"RESULT data={args.data_secs} rate={args.rate}/{args.bits}bit burst={args.burst} drain={args.drain}: "
          f"first sound ~{first_play - 1 - state['start']:.2f}s after stream start, "
          f"steady delay {min(lags):.2f}-{max(lags):.2f}s (median {sorted(lags)[len(lags)//2]:.2f}s), "
          f"position stalls: {stalls}, buflvl seen: {sorted(set(results['buflvl']))}")
else:
    print("RESULT: the speaker did not play", pos[:5])

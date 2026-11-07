#!/usr/bin/env python3
"""Feasibility test: play a live (endless, real-time paced) audio stream on a
DLNA renderer and measure how long it buffers before playing.

The audio comes from ffmpeg decoding a file in real time (-re), which behaves
like live capture: the server can never send faster than real time.

Usage:
  python3 -I stream_test.py (<audio-file> | --capture) [--renderer 192.168.50.100] [--me 192.168.50.67]
                            [--format wav|l16] [--seconds 60]
Logs: request/first-byte times, bytes served per second, and the renderer's
own play state from its port-10100 display stream (KEF LS50W specific).
"""
import argparse
import html
import os
import tempfile
import re
import socket
import subprocess
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

p = argparse.ArgumentParser()
p.add_argument("file", nargs="?")
p.add_argument("--capture", action="store_true", help="stream the Mac's audio output (capture/Capture.app)")
p.add_argument("--renderer", default="192.168.50.100")
p.add_argument("--me", default="192.168.50.67")
p.add_argument("--port", type=int, default=8300)
p.add_argument("--format", choices=["wav", "l16"], default="wav")
p.add_argument("--seconds", type=float, default=60)
p.add_argument("--rate", type=int, default=44100)
p.add_argument("--bits", type=int, choices=[16, 24], default=16)
p.add_argument("--nodelay", action="store_true", help="TCP_NODELAY + one write per 5 ms (like ls50streamd 0.1.0)")
args = p.parse_args()
if not args.file and not args.capture:
    p.error("give an audio file or --capture")
CAPTURE_APP = os.path.join(os.path.dirname(os.path.abspath(__file__)), "capture", "Capture.app")

T0 = time.time()
def log(*a):
    print(f"+{time.time() - T0:6.2f}s", *a, flush=True)

FORMATS = {
    # Endless WAV: ffmpeg writes a header with unknown (max) sizes when piping.
    "wav": ("audio/wav", ["-f", "wav", "-acodec", f"pcm_s{args.bits}le"], "DLNA.ORG_PN=WAV;" if args.bits == 16 else ""),
    # Raw big-endian PCM, the DLNA "LPCM" profile.
    "l16": (f"audio/L16;rate={args.rate};channels=2", ["-f", "s16be"], "DLNA.ORG_PN=LPCM;"),
}
MIME, FFARGS, PN = FORMATS[args.format]
FLAGS = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
URL = f"http://{args.me}:{args.port}/live.{args.format}"
BYTES_PER_SEC = args.rate * 2 * args.bits // 8


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        pass

    def headers_out(self):
        self.send_response(200)
        self.send_header("Content-Type", MIME)
        self.send_header("transferMode.dlna.org", "Streaming")
        self.send_header("contentFeatures.dlna.org", PN + FLAGS)
        self.send_header("Connection", "close")
        self.end_headers()

    def do_HEAD(self):
        log("HEAD", self.path, dict(self.headers))
        self.headers_out()

    def do_GET(self):
        log("GET", self.path, "Range:", self.headers.get("Range"), "UA:", self.headers.get("User-Agent"))
        self.headers_out()
        if args.capture:
            # Capture.app writes float32 stereo into a FIFO; ffmpeg reads it unbuffered.
            fifo = os.path.join(tempfile.mkdtemp(), "capture.f32")
            os.mkfifo(fifo)
            subprocess.run(["open", "-n", CAPTURE_APP, "--stdout", fifo, "--stderr", fifo + ".log"], check=True)
            source = ["-f", "f32le", "-ar", str(CAPTURE_RATE), "-ac", "2", "-probesize", "32",
                      "-analyzeduration", "0", "-fflags", "nobuffer", "-i", fifo]
        else:
            source = ["-re", "-i", args.file]
        ff = subprocess.Popen(
            ["ffmpeg", "-loglevel", "error", *source, "-ar", str(args.rate), "-ac", "2", *FFARGS,
             "-flush_packets", "1", "pipe:1"],
            stdout=subprocess.PIPE)
        if args.nodelay:
            self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        chunk_bytes = BYTES_PER_SEC // 200 // 6 * 6 if args.nodelay else 4096
        sent, first, last_report = 0, None, time.time()
        try:
            while chunk := ff.stdout.read(chunk_bytes):
                self.wfile.write(chunk)
                sent += len(chunk)
                if first is None:
                    first = time.time()
                    log("first byte sent")
                if time.time() - last_report >= 5:
                    log(f"served {sent / BYTES_PER_SEC:.1f}s of audio in {time.time() - first:.1f}s")
                    last_report = time.time()
        except (BrokenPipeError, ConnectionResetError):
            log(f"renderer closed the stream after {sent / BYTES_PER_SEC:.1f}s of audio")
        finally:
            ff.kill()
            if args.capture:
                # Capture.app unmutes the Mac's output when it exits.
                subprocess.run(["pkill", "-INT", "-f", "Capture.app/Contents/MacOS/capture"])


def soap(action, body):
    envelope = (
        '<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
        f'<u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>{body}</u:{action}>'
        '</s:Body></s:Envelope>')
    req = urllib.request.Request(
        f"http://{args.renderer}:8080/AVTransport/ctrl", data=envelope.encode(),
        headers={"Content-Type": 'text/xml; charset="utf-8"',
                 "SOAPACTION": f'"urn:schemas-upnp-org:service:AVTransport:1#{action}"'})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def watch_display():
    """Logs changes of the renderer's play icon and position (KEF port 10100)."""
    s = socket.create_connection((args.renderer, 10100), timeout=3)
    s.settimeout(0.2)
    buf, icon, pos = "", None, None
    while True:
        try:
            buf += s.recv(65536).decode("utf-8", "replace")
        except socket.timeout:
            continue
        # Handle complete fragments only, each once.
        while (m := re.search(r"<(view|update) id=[^>]*>.*?</\1>", buf, re.S)):
            frag, buf = m.group(0), buf[m.end():]
            if (i := re.search(r'id="play">([^<]*)<', frag)) and i.group(1).strip() != icon:
                icon = i.group(1).strip()
                log("renderer play icon:", icon or "(none)")
            if (t := re.search(r'id="timep"[^>]*>(\d+)<', frag)) and t.group(1) != pos:
                pos = t.group(1)
                if pos in ("0", "1", "5", "10", "20"):
                    log("renderer position", pos, "s")


if args.capture:
    CAPTURE_RATE = int(subprocess.run([os.path.join(CAPTURE_APP, "Contents/MacOS/capture"), "--format"],
                                      capture_output=True, text=True, check=True).stdout)
    log(f"capturing the Mac's output at {CAPTURE_RATE} Hz")

server = ThreadingHTTPServer((args.me, args.port), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Thread(target=watch_display, daemon=True).start()

meta = (f'<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" '
        f'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="live" parentID="0" restricted="1">'
        f'<dc:title>Mac audio</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>'
        f'<res protocolInfo="http-get:*:{MIME}:{PN}{FLAGS}">{URL}</res></item></DIDL-Lite>')
log("SetAVTransportURI", URL)
status, body = soap("SetAVTransportURI",
                    f"<CurrentURI>{URL}</CurrentURI><CurrentURIMetaData>{html.escape(meta)}</CurrentURIMetaData>")
log("->", status, re.findall(r"<(?:faultstring|errorDescription)>([^<]*)", body) or "OK")
status, body = soap("Play", "<Speed>1</Speed>")
log("Play ->", status, re.findall(r"<(?:faultstring|errorDescription)>([^<]*)", body) or "OK")

time.sleep(args.seconds)
log("Stop ->", soap("Stop", "")[0])

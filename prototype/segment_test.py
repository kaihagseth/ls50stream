#!/usr/bin/env python3
"""Can the speaker play an endless live stream as a chain of finite WAV "tracks"?

The WAV header's data size is honoured strictly (the speaker hangs up after it),
so a live stream must be cut into segments. Each segment declares --seg seconds
and carries exactly the next part of the live audio; the next segment is queued
with SetNextAVTransportURI as soon as the speaker fetches the current one.
Logs fetches and the speaker's play position (10100 `timep`) to show gaps.

  python3 -I segment_test.py <file> [--rate 48000] [--seg 10] [--seconds 40]
"""
import argparse
import html
import re
import socket
import struct
import subprocess
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

p = argparse.ArgumentParser()
p.add_argument("file")
p.add_argument("--renderer", default="192.168.50.100")
p.add_argument("--port", type=int, default=8304)
p.add_argument("--rate", type=int, default=48000)
p.add_argument("--seg", type=float, default=10.0)
p.add_argument("--seconds", type=float, default=40.0)
args = p.parse_args()

FRAME = 6
BPS = args.rate * FRAME
SEG_BYTES = int(args.seg * args.rate) * FRAME
probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
probe.connect((args.renderer, 9))
ME = probe.getsockname()[0]
T0 = time.time()
live_start = None  # live audio position 0 = first fetch of segment 0

def log(*a):
    print(f"+{time.time() - T0:6.2f}s", *a, flush=True)

source = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", args.file, "-t", str(args.seconds + 20),
                         "-ar", str(args.rate), "-ac", "2", "-acodec", "pcm_s24le", "-f", "s24le", "pipe:1"],
                        capture_output=True, check=True).stdout

def header():
    return (b"RIFF" + struct.pack("<I", SEG_BYTES + 60) + b"WAVEfmt " +
            struct.pack("<IHHIIHHHHI", 40, 0xFFFE, 2, args.rate, BPS, FRAME, 24, 22, 24, 3) +
            bytes([0x01, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]) +
            b"data" + struct.pack("<I", SEG_BYTES))

def url(k):
    return f"http://{ME}:{args.port}/seg/{k}.wav"

def soap_text(action, body=""):
    env = (f'<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
           f's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">'
           f'<InstanceID>0</InstanceID>{body}</u:{action}></s:Body></s:Envelope>')
    req = urllib.request.Request(f"http://{args.renderer}:8080/AVTransport/ctrl", data=env.encode(),
                                 headers={"Content-Type": 'text/xml; charset="utf-8"',
                                          "SOAPACTION": f'"urn:schemas-upnp-org:service:AVTransport:1#{action}"'})
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.read().decode()

def keep_next_queued():
    """Queue segment k+1 whenever the speaker plays segment k and its next slot is empty."""
    while True:
        time.sleep(1)
        try:
            info = soap_text("GetMediaInfo")
        except Exception as e:
            continue
        cur = re.search(r"<CurrentURI>[^<]*/seg/(\d+)\.wav", info)
        nxt = re.search(r"<NextURI>([^<]*)</NextURI>", info)
        if cur and (not nxt or not nxt.group(1).strip()):
            k = int(cur.group(1)) + 1
            if k not in queued:
                queued.add(k)
                log(f"speaker on segment {k - 1}, next slot empty: queueing segment {k}:",
                    soap("SetNextAVTransportURI", f"<NextURI>{url(k)}</NextURI><NextURIMetaData>{meta(k)}</NextURIMetaData>"))

def soap(action, body):
    env = (f'<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
           f's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">'
           f'<InstanceID>0</InstanceID>{body}</u:{action}></s:Body></s:Envelope>')
    req = urllib.request.Request(f"http://{args.renderer}:8080/AVTransport/ctrl", data=env.encode(),
                                 headers={"Content-Type": 'text/xml; charset="utf-8"',
                                          "SOAPACTION": f'"urn:schemas-upnp-org:service:AVTransport:1#{action}"'})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code

def meta(k):
    d = (f'<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" '
         f'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="s{k}" parentID="0" restricted="1">'
         f'<dc:title>Mac</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>'
         f'<res protocolInfo="http-get:*:audio/wav:DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000">{url(k)}</res>'
         f'</item></DIDL-Lite>')
    return html.escape(d)

queued = set()

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        global live_start
        k = int(re.search(r"/seg/(\d+)\.wav", self.path).group(1))
        log(f"GET segment {k} Range={self.headers.get('Range')} {self.headers.get('User-Agent')}")
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(SEG_BYTES + 68))
        self.end_headers()
        self.wfile.write(header())
        if live_start is None:
            live_start = time.time()
        start, end, pos = k * SEG_BYTES, (k + 1) * SEG_BYTES, k * SEG_BYTES
        try:
            while pos < end:
                live = int((time.time() - live_start) * args.rate) * FRAME
                upto = min(live, end)
                if upto > pos:
                    self.wfile.write(source[pos % len(source):upto % len(source) or None] if upto // len(source) == pos // len(source) else source[pos % len(source):])
                    pos = upto
                time.sleep(0.01)
            log(f"segment {k} fully sent")
        except (BrokenPipeError, ConnectionResetError):
            log(f"segment {k}: speaker closed after {(pos - start) / BPS:.1f}s")

def watch():
    s = socket.create_connection((args.renderer, 10100), timeout=3)
    s.settimeout(0.1)
    buf, last = "", None
    while True:
        try:
            buf += s.recv(65536).decode("utf-8", "replace")
        except socket.timeout:
            continue
        while (m := re.search(r"<(view|update) id=[^>]*>.*?</\1>", buf, re.S)):
            frag, buf = m.group(0), buf[m.end():]
            if (t := re.search(r'timep[^>]*>(\d+)<', frag)) and t.group(1) != last:
                last = t.group(1)
                log(f"  position {last}")
            if (i := re.search(r'id="play">([^<]*)<', frag)) and i.group(1).strip() not in ("", "play:4"):
                log(f"  play icon {i.group(1).strip()}")

server = ThreadingHTTPServer((ME, args.port), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
threading.Thread(target=watch, daemon=True).start()
threading.Thread(target=keep_next_queued, daemon=True).start()
log(f"segments of {args.seg}s at {args.rate} Hz")
queued.add(0)
log("SetAVTransportURI:", soap("SetAVTransportURI", f"<CurrentURI>{url(0)}</CurrentURI><CurrentURIMetaData>{meta(0)}</CurrentURIMetaData>"))
log("Play:", soap("Play", "<Speed>1</Speed>"))
time.sleep(args.seconds)
log("Stop:", soap("Stop", ""))

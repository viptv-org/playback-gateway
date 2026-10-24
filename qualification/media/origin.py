#!/usr/bin/env python3
"""Owned HTTPS file origin with bounded Range responses and observable delivery."""

import argparse
import bisect
import json
import mimetypes
import hashlib
from pathlib import Path
import re
import ssl
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit, parse_qs


def byte_range(value, size):
    if value is None:
        return 0, size - 1, False
    match = re.fullmatch(r"bytes=(\d*)-(\d*)", value)
    if not match or not any(match.groups()) or size == 0:
        raise ValueError("invalid range")
    first, last = match.groups()
    if first:
        start, end = int(first), min(int(last), size - 1) if last else size - 1
    else:
        if int(last) == 0:
            raise ValueError("empty suffix")
        start, end = max(0, size - int(last)), size - 1
    if start >= size or end < start:
        raise ValueError("unsatisfiable range")
    return start, end, True


class Origin(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self, address, root, events, delay_ms=0, bytes_per_second=0, header=None
    ):
        super().__init__(address, Handler)
        self.root = Path(root).resolve()
        self.events = events
        self.delay = delay_ms / 1000
        self.rate = bytes_per_second
        self.required_header = header
        self.write_lock = threading.Lock()
        self.started = time.monotonic()
        self.live_epochs = {}
        self.live_publications = {}
        self.ts_indexes = {}

    def record(self, item):
        if self.events:
            with self.write_lock, Path(self.events).open("a") as f:
                f.write(json.dumps(item, sort_keys=True) + "\n")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def log_message(self, *_):
        pass  # Request paths/headers are deliberately absent from diagnostics.

    def do_HEAD(self):
        self.serve(head=True)

    def do_GET(self):
        self.serve(head=False)

    def empty(self, status, headers=None):
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def serve(self, head):
        started = time.monotonic()
        sent, status = 0, 500
        fixture = "unknown"
        asset = None
        source_epoch = None
        source_lead = None
        source_elapsed = None
        try:
            path = unquote(urlsplit(self.path).path)
            paced = path.startswith("/paced/")
            if paced:
                path = path[len("/paced") :]
            if path.startswith("/redirect/"):
                status = 307
                self.empty(status, {"Location": "/" + path[len("/redirect/") :]})
                return
            if self.server.required_header:
                key, value = self.server.required_header
                if self.headers.get(key) != value:
                    status = 403
                    self.empty(status)
                    return
            target = (self.server.root / path.lstrip("/")).resolve()
            if (
                not target.is_relative_to(self.server.root)
                or not target.is_file()
                or target.suffix in (".json", ".log", ".key")
            ):
                status = 404
                self.empty(status)
                return
            fixture = target.relative_to(self.server.root).parts[0]
            asset = target.name
            query = urlsplit(self.path).query
            try:
                profile = parse_qs(query)
                age = float(profile.get("age", ["0"])[0])
                window = int(profile.get("window", ["12"])[0])
                if not 0 <= age <= 7200 or not 12 <= window <= 180:
                    raise ValueError()
            except ValueError:
                status = 400
                self.empty(status)
                return
            lead = window if window != 12 else 10
            if path.startswith("/live/") and target.suffix in (".m3u8", ".mpd"):
                from live import hls_window, dash_window

                with self.server.write_lock:
                    # A new owned campaign gets a fresh source clock; every
                    # viewer within that campaign observes the same timeline.
                    epoch = self.server.live_epochs.setdefault(
                        (str(target), query), time.time() - age
                    )
                elapsed = time.time() - epoch
                source_elapsed = elapsed
                source_epoch = epoch
                if target.suffix == ".m3u8":
                    from live import hls_segments

                    source_lead = hls_segments(target.read_text())[3]
                else:
                    source_lead = lead
                body = (
                    hls_window(target.read_text(), elapsed)
                    if target.suffix == ".m3u8"
                    else dash_window(
                        target.read_text(), elapsed, epoch, query, window, lead
                    )
                )
                if query and target.suffix == ".m3u8":
                    body = (
                        "\n".join(
                            line + "?" + query
                            if line and not line.startswith("#")
                            else line
                            for line in body.decode().splitlines()
                        )
                        + "\n"
                    ).encode()
                status = 200
                self.send_response(status)
                self.send_header(
                    "Content-Type",
                    "application/vnd.apple.mpegurl"
                    if target.suffix == ".m3u8"
                    else "application/dash+xml",
                )
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if not head:
                    self.wfile.write(body)
                    sent = len(body)
                return
            if (
                path.startswith("/live/")
                and target.suffix in (".m4s", ".ts")
                and not paced
            ):
                from live import publication_times

                manifest = target.parent / (
                    "index.mpd" if target.suffix == ".m4s" else "index.m3u8"
                )
                with self.server.write_lock:
                    epoch = self.server.live_epochs.get((str(manifest), query))
                    schedule = self.server.live_publications.get((str(manifest), lead))
                    if schedule is None:
                        schedule = publication_times(
                            manifest.read_text(), manifest.suffix, lead
                        )
                        self.server.live_publications[(str(manifest), lead)] = schedule
                due = schedule.get(target.name)
                # Initialization data is always available. Media is published
                # only when complete; a request for the next segment can wait
                # one segment interval, but cannot consume the future corpus.
                if due is not None:
                    wait = due - (time.time() - epoch) if epoch is not None else 999
                    if wait > 4:
                        status = 404
                        self.empty(status)
                        return
                    if wait > 0:
                        time.sleep(wait)
            size = target.stat().st_size
            if paced:
                metadata = json.loads((target.parent / "live.json").read_text())
                rate = size / metadata["duration"]
                status = 200
                self.send_response(status)
                self.send_header("Content-Type", "video/mp2t")
                self.send_header("Accept-Ranges", "none")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                if head:
                    return
                index_file = target.parent / "keyframes.json"
                if index_file.exists():
                    with self.server.write_lock:
                        index = self.server.ts_indexes.get(str(target))
                        if index is None:
                            index = json.loads(index_file.read_text())
                            assert index["source_sha256"] == metadata["manifest_sha256"]
                            self.server.ts_indexes[str(target)] = index
                        source_epoch = self.server.live_epochs.setdefault(
                            (str(target), query), time.time() - age
                        )
                    source_lead = 6
                    entries = index["entries"]
                    times = [entry[0] for entry in entries]
                    elapsed = time.time() - source_epoch + source_lead
                    source_elapsed = elapsed - source_lead
                    if elapsed >= metadata["duration"]:
                        self.wfile.write(b"0\r\n\r\n")
                        return
                    first = max(0, bisect.bisect_right(times, elapsed) - 1)
                    with target.open("rb") as stream:
                        for n in range(first, len(entries)):
                            start_time, offset = entries[n]
                            end_time, end_offset = (
                                entries[n + 1]
                                if n + 1 < len(entries)
                                else (metadata["duration"], size)
                            )
                            stream.seek(offset)
                            remaining = end_offset - offset
                            delivered = 0
                            while remaining:
                                block = stream.read(min(188 * 16, remaining))
                                if not block:
                                    raise OSError("clock TS truncated")
                                delivered += len(block)
                                remaining -= len(block)
                                due = (
                                    source_epoch
                                    - source_lead
                                    + start_time
                                    + (end_time - start_time)
                                    * delivered
                                    / (end_offset - offset)
                                )
                                wait = due - time.time()
                                if wait > 0:
                                    time.sleep(wait)
                                self.wfile.write(
                                    f"{len(block):x}\r\n".encode() + block + b"\r\n"
                                )
                                self.wfile.flush()
                                sent += len(block)
                    self.wfile.write(b"0\r\n\r\n")
                    return
                began = time.monotonic()
                with target.open("rb") as stream:
                    while block := stream.read(188 * 32):
                        self.wfile.write(
                            f"{len(block):x}\r\n".encode() + block + b"\r\n"
                        )
                        self.wfile.flush()
                        sent += len(block)
                        wait = sent / rate - (time.monotonic() - began)
                        if wait > 0:
                            time.sleep(wait)
                self.wfile.write(b"0\r\n\r\n")
                return
            try:
                start, end, partial = byte_range(self.headers.get("Range"), size)
            except ValueError:
                status = 416
                self.empty(status, {"Content-Range": f"bytes */{size}"})
                return
            if self.server.delay:
                time.sleep(self.server.delay)
            status = 206 if partial else 200
            self.send_response(status)
            self.send_header(
                "Content-Type",
                {
                    ".m3u8": "application/vnd.apple.mpegurl",
                    ".mpd": "application/dash+xml",
                    ".ts": "video/mp2t",
                    ".m4s": "video/iso.segment",
                }.get(
                    target.suffix,
                    mimetypes.guess_type(target.name)[0] or "application/octet-stream",
                ),
            )
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(end - start + 1))
            if partial:
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.end_headers()
            if head:
                return
            with target.open("rb") as f:
                f.seek(start)
                remaining = end - start + 1
                transfer_start = time.monotonic()
                while remaining:
                    block = f.read(min(64 * 1024, remaining))
                    if not block:
                        raise OSError("fixture truncated during delivery")
                    self.wfile.write(block)
                    sent += len(block)
                    remaining -= len(block)
                    if self.server.rate:
                        delay = sent / self.server.rate - (
                            time.monotonic() - transfer_start
                        )
                        if delay > 0:
                            time.sleep(delay)
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            pass
        finally:
            self.server.record(
                dict(
                    fixture=fixture,
                    asset=asset,
                    source_epoch=source_epoch,
                    source_lead_seconds=source_lead,
                    source_elapsed_seconds=source_elapsed,
                    query_sha256=hashlib.sha256(
                        urlsplit(self.path).query.encode()
                    ).hexdigest()[:16],
                    method=self.command,
                    status=status,
                    bytes=sent,
                    elapsed_ms=(time.monotonic() - started) * 1000,
                    monotonic_ns=time.monotonic_ns(),
                    wall_ns=time.time_ns(),
                )
            )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--root", type=Path, required=True)
    p.add_argument("--cert", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--bind", default="127.0.0.1")
    p.add_argument("--port", type=int, default=18443)
    p.add_argument("--events", type=Path, required=True)
    p.add_argument("--delay-ms", type=float, default=0)
    p.add_argument("--bytes-per-second", type=int, default=0)
    p.add_argument("--header-file", type=Path)
    args = p.parse_args()
    server = Origin(
        (args.bind, args.port),
        args.root,
        args.events,
        args.delay_ms,
        args.bytes_per_second,
        json.loads(args.header_file.read_text()) if args.header_file else None,
    )
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(args.cert, args.key)
    server.socket = tls.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


if __name__ == "__main__":
    main()

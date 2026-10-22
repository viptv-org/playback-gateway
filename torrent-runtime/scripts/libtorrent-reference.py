#!/usr/bin/env python3
"""BENCHMARK ONLY: verified libtorrent deadline reference, not a runtime replacement.

Requires libtorrent==2.1.1 in an isolated Python environment. Rejects torrents
larger than the budget: the public handle API has no per-piece invalidation for
rolling eviction. Does not implement the production authority/archive contract.
Never package this executable in a client or gateway.
"""
import base64
from collections import OrderedDict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import re
import secrets
import sys
import threading
import time

import libtorrent as lt


class Reference:
    def __init__(self, config):
        self.limit = config.get("cache_bytes", 2<<30)
        self.root = config["cache_dir"]; os.makedirs(self.root, exist_ok=True)
        self.session = lt.session({"enable_upnp": False, "enable_natpmp": False,
            "enable_lsd": False, "connections_limit": 55, "alert_mask": int(lt.alert.category_t.all_categories)})
        self.token = secrets.token_hex(24)
        self.condition = threading.Condition(); self.closed = False
        self.handle = None; self.info = None; self.error = None
        self.reads = OrderedDict(); self.reading = set(); self.demands = {}; self.priorities = set()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.alerts, daemon=True); self.thread.start()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def alerts(self):
        while not self.closed:
            self.session.wait_for_alert(100)
            for alert in self.session.pop_alerts():
                if isinstance(alert, lt.read_piece_alert):
                    with self.condition:
                        self.reading.discard(alert.piece)
                        if alert.error.value() == 0:
                            self.reads[alert.piece] = bytes(alert.buffer)
                            while len(self.reads)>8: self.reads.popitem(last=False)
                        self.condition.notify_all()

    def prepare(self, source):
        self.source = source
        if source.get("metainfo"):
            params = lt.add_torrent_params(); params.ti = lt.torrent_info(lt.bdecode(base64.b64decode(source["metainfo"])))
        else: params = lt.parse_magnet_uri(source["magnet"])
        params.save_path = self.root; params.trackers = source.get("trackers", [])
        params.flags &= ~lt.torrent_flags.auto_managed
        params.flags &= ~lt.torrent_flags.paused
        params.flags |= lt.torrent_flags.default_dont_download
        self.handle = self.session.add_torrent(params)

    def observe(self):
        if self.handle is None: return {"stage": "closed", "error": "closed"}
        if not self.handle.status().has_metadata: return {"stage": "finding_peers", "ready": False}
        if self.info is None:
            self.info = self.handle.torrent_file(); files = self.info.files()
            if self.info.total_size() > self.limit: self.error = "reference_requires_nonrolling_storage"
            self.index = self.source.get("file_index")
            if self.index is None: self.index = max(range(files.num_files()), key=files.file_size)
            self.length = files.file_size(self.index); self.offset = files.file_offset(self.index)
            self.handle.prioritize_pieces([0]*self.info.num_pieces())
        if self.error: return {"stage": "failed", "error": self.error}
        return {"stage": "buffering", "ready": True, "length": self.length, "file_index": self.index}

    def demand(self, reader, piece):
        # Deadlines prioritize verified complete pieces, never unverified blocks.
        with self.condition:
            if piece is None: self.demands.pop(reader, None)
            else: self.demands[reader] = set(range(piece, min(piece+4, self.info.num_pieces())))
            wanted = set().union(*self.demands.values()) if self.demands else set()
            for old in self.priorities-wanted:
                self.handle.reset_piece_deadline(old); self.handle.piece_priority(old, 0)
            for new in wanted-self.priorities:
                self.handle.piece_priority(new, 7)
                self.handle.set_piece_deadline(new, 0 if new == piece else 100)
            self.priorities = wanted

    def piece(self, index):
        deadline = time.monotonic()+90
        with self.condition:
            while not self.closed:
                if index in self.reads:
                    self.reads.move_to_end(index); return self.reads[index]
                if self.handle.have_piece(index) and index not in self.reading:
                    self.reading.add(index); self.handle.read_piece(index)
                if time.monotonic()>deadline: raise TimeoutError()
                self.condition.wait(.02)
        raise EOFError()

    def handler(self):
        reference = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_HEAD(self): self.serve(False)
            def do_GET(self): self.serve(True)
            def serve(self, body):
                if self.path != '/'+reference.token or reference.closed:
                    self.send_error(404); return
                start, end = 0, reference.length-1
                value = self.headers.get("Range")
                if value:
                    match = re.fullmatch(r"bytes=(\d*)-(\d*)", value)
                    if not match or not any(match.groups()): self.send_error(416); return
                    a,b=match.groups()
                    if not a: start=max(0,reference.length-int(b))
                    else: start=int(a); end=min(end,int(b)) if b else end
                    if start>end: self.send_error(416); return
                self.send_response(206 if value else 200)
                self.send_header("Content-Length",str(end-start+1)); self.send_header("Accept-Ranges","bytes")
                if value: self.send_header("Content-Range",f"bytes {start}-{end}/{reference.length}")
                self.end_headers()
                if not body: return
                reader = object()
                try:
                    offset = start
                    while offset<=end:
                        absolute=reference.offset+offset
                        index=absolute//reference.info.piece_length()
                        relative=absolute%reference.info.piece_length()
                        reference.demand(reader,index)
                        data=reference.piece(index)
                        count=min(end-offset+1,len(data)-relative)
                        self.wfile.write(data[relative:relative+count]); offset+=count
                except (OSError,EOFError,TimeoutError): pass
                finally:
                    if not reference.closed: reference.demand(reader,None)
        return Handler

    def close(self):
        self.closed = True
        with self.condition: self.condition.notify_all()
        self.server.shutdown(); self.server.server_close()
        if self.handle is not None: self.session.remove_torrent(self.handle)
        self.thread.join(2)


def main():
    runtime=None
    print(json.dumps({"version":2,"ready":True,"qualification":"libtorrent-reference-only","libtorrent":lt.version}),flush=True)
    try:
        for line in sys.stdin:
            response={"version":2,"ok":True}
            try:
                request=json.loads(line); op=request['op']
                if op=='open': runtime=Reference(request['config'])
                elif op=='prepare': runtime.prepare(request['source']);response['handle']='1'
                elif op=='observe':
                    response['progress']=runtime.observe()
                    if response['progress'].get('ready'):response['media_url']=f'http://127.0.0.1:{runtime.server.server_port}/{runtime.token}'
                elif op in ('renew','first_frame'): pass
                elif op=='close': runtime.close();runtime=None
                elif op=='shutdown':
                    if runtime:runtime.close();runtime=None
                else:raise ValueError()
            except Exception:
                response={'version':2,'ok':False,'error':'reference_failed'}
            print(json.dumps(response),flush=True)
    finally:
        if runtime: runtime.close()


if __name__=='__main__':main()

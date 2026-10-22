#!/usr/bin/env python3
"""Own a generated movie, throttled peer and tracker for desktop comparisons.

--baseline/--candidate must be test-only private-network workers. Production
destination policy is unchanged. Invoke this script under a managed display.
"""
import argparse
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import selectors
import struct
import subprocess
import threading


def bencode(value):
    if isinstance(value, int): return b'i'+str(value).encode()+b'e'
    if isinstance(value, bytes): return str(len(value)).encode()+b':'+value
    if isinstance(value, list): return b'l'+b''.join(map(bencode,value))+b'e'
    return b'd'+b''.join(bencode(k)+bencode(value[k]) for k in sorted(value))+b'e'


def bdecode(raw):
    pos=0
    def read():
        nonlocal pos
        char=raw[pos:pos+1]
        if char==b'i': end=raw.index(b'e',pos); n=int(raw[pos+1:end]);pos=end+1;return n
        if char in (b'l',b'd'):
            pos+=1;out=[] if char==b'l' else {}
            while raw[pos:pos+1]!=b'e':
                if char==b'l':out.append(read())
                else:key=read();out[key]=read()
            pos+=1;return out
        end=raw.index(b':',pos);size=int(raw[pos:end]);pos=end+1
        data=raw[pos:pos+size];pos+=size;return data
    return read()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seeder',type=Path,required=True)
    parser.add_argument('--baseline',type=Path,required=True);parser.add_argument('--candidate',type=Path,required=True)
    parser.add_argument('--launcher',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--piece-bytes',type=int,default=4<<20)
    parser.add_argument('--upload-bytes',type=int,default=1<<20)
    parser.add_argument('--trials',type=int,default=3)
    parser.add_argument('--engines',nargs='+',default=['baseline','candidate','stremio'])
    parser.add_argument('--startup-only',action='store_true')
    args=parser.parse_args();os.umask(0o077)
    args.output.mkdir(parents=True,exist_ok=False)
    media=args.output/'generated.mp4'
    with (args.output/'encode.log').open('wb') as log:
        subprocess.run(['ffmpeg','-nostdin','-hide_banner','-loglevel','error','-f','lavfi','-i',
            'testsrc2=size=640x360:rate=24:duration=140','-f','lavfi','-i','sine=frequency=440:sample_rate=48000:duration=140',
            '-c:v','libx264','-preset','veryfast','-threads','2','-b:v','1M','-maxrate','1M','-bufsize','2M',
            '-g','48','-c:a','aac','-movflags','+faststart','-shortest',str(media)],stdout=log,stderr=log,check=True,timeout=120)
    env=dict(os.environ,TORRENT_OWNED_SEED_FILE=str(media),TORRENT_OWNED_PIECE_BYTES=str(args.piece_bytes),
             TORRENT_OWNED_UPLOAD_BYTES=str(args.upload_bytes))
    seedlog=(args.output/'seed.log').open('wb')
    seed=subprocess.Popen([str(args.seeder),'-test.run','^TestGatewayQualificationSeeder$'],env=env,
                          stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=seedlog)
    tracker=None
    try:
        with selectors.DefaultSelector() as ready:
            ready.register(seed.stdout,selectors.EVENT_READ)
            if not ready.select(30):raise TimeoutError('owned seeder startup')
        announcement=json.loads(seed.stdout.readline())
        peers=b'\x7f\x00\x00\x01'+struct.pack('!H',announcement['peer_port'])
        class Tracker(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                data=bencode({b'interval':60,b'complete':1,b'incomplete':0,b'peers':peers})
                self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        tracker=ThreadingHTTPServer(('127.0.0.1',0),Tracker)
        threading.Thread(target=tracker.serve_forever,daemon=True).start()
        metainfo=bdecode(base64.b64decode(announcement['metainfo']))
        url=f'http://127.0.0.1:{tracker.server_port}/announce'.encode()
        metainfo[b'announce']=url;metainfo[b'announce-list']=[[url]]
        torrent=args.output/'generated.torrent';torrent.write_bytes(bencode(metainfo))
        corpus=args.output/'corpus.json';corpus.write_text(json.dumps({'sources':[{'torrent':str(torrent)}]}))
        (args.output/'fixture.json').write_text(json.dumps({'piece_bytes':args.piece_bytes,'upload_bytes_per_second':args.upload_bytes,
            'duration_seconds':140,'file_bytes':media.stat().st_size,'source':'generated test pattern and tone','worker_scope':'test-only private network'}))
        subprocess.run(['python3',str(Path(__file__).with_name('benchmark-desktop.py')),'--launcher',str(args.launcher),
            '--baseline',str(args.baseline),'--candidate',str(args.candidate),'--corpus',str(corpus),
            '--output',str(args.output/'comparison'),'--trials',str(args.trials),'--states',*(['cold'] if args.startup_only else ['cold','retained']),'--timeout','45','--engines',*args.engines,*(['--startup-only'] if args.startup_only else [])],check=True)
    finally:
        if tracker:tracker.shutdown();tracker.server_close()
        seed.stdin.close()
        try:seed.wait(5)
        except subprocess.TimeoutExpired:seed.kill();seed.wait()
        seedlog.close()


if __name__=='__main__':main()

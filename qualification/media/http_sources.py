#!/usr/bin/env python3
"""Bounded HTTPS preflight for private source manifests; reports never contain URLs."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import time
import urllib.error
import urllib.parse
import urllib.request


def signature(body):
    if len(body) >= 8 and body[4:8] == b'ftyp':return 'iso-bmff'
    if body[:4] == b'\x1aE\xdf\xa3':return 'matroska'
    if body.lstrip().startswith(b'#EXTM3U'):return 'hls'
    if body[:4] == b'OggS':return 'ogg'
    if body[:3] == b'FLV':return 'flv'
    if body[:4] == b'RIFF' and body[8:12] == b'AVI ':return 'avi'
    if body[:4] == b'\0\0\1\xba':return 'mpeg-ps'
    for offset,stride,label in [(0,188,'mpeg-ts'),(4,192,'m2ts')]:
        if len(body)>offset+2*stride and all(body[offset+i*stride]==0x47 for i in range(3)):
            return label
    return 'unknown'


class Redirects(urllib.request.HTTPRedirectHandler):
    def __init__(self):
        self.hops = []

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        parsed = urllib.parse.urlsplit(newurl)
        if parsed.scheme != 'https' or parsed.username or parsed.password:
            raise ValueError('redirect_requires_https_without_userinfo')
        self.hops.append(dict(status=code, host=parsed.hostname))
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is not None and (
            urllib.parse.urlsplit(req.full_url).netloc != parsed.netloc
        ):
            for name in ('Authorization', 'Cookie', 'Proxy-Authorization'):
                redirected.remove_header(name)
        return redirected


def check(item, timeout):
    row = dict(id=item['id'], title=item.get('title', item['id']), requests=[])
    for method in ('HEAD', 'GET'):
        redirects = Redirects()
        opener = urllib.request.build_opener(redirects)
        started = time.monotonic()
        result = dict(method=method, redirects=redirects.hops)
        headers = {'User-Agent': 'VIPTV-media-qualification/1', **item.get('headers', {})}
        if method == 'GET':
            headers['Range'] = 'bytes=0-65535'
        try:
            parsed = urllib.parse.urlsplit(item['url'])
            if parsed.scheme != 'https' or parsed.username or parsed.password:
                raise ValueError('source_requires_https')
            req = urllib.request.Request(item['url'], method=method, headers=headers)
            try:
                response = opener.open(req, timeout=timeout)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                result.update(status=response.status,
                              final_host=urllib.parse.urlsplit(response.url).hostname,
                              content_type=response.headers.get('Content-Type'),
                              content_length=response.headers.get('Content-Length'),
                              content_range=response.headers.get('Content-Range'),
                              accept_ranges=response.headers.get('Accept-Ranges'))
                body = response.read(65536) if method == 'GET' else b''
                result['bytes_read'] = len(body)
                if body:
                    result['prefix_sha256'] = hashlib.sha256(body).hexdigest()
                    result['media_signature'] = signature(body)
                result['tls_verified'] = True
        except Exception as error:
            # Exception messages may contain full credential-bearing URLs.
            result.update(error_type=type(error).__name__, tls_verified=False)
        result['elapsed_ms'] = round((time.monotonic() - started) * 1000, 2)
        row['requests'].append(result)
    get = row['requests'][-1]
    row['status'] = ('reachable_media' if get.get('status') in (200, 206) and
                     get.get('media_signature','unknown') != 'unknown' else 'upstream_failed')
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=float, default=20)
    parser.add_argument('--workers', type=int, default=2)
    args = parser.parse_args()
    items = json.loads(args.manifest.read_text())['fixtures']
    assert 1 <= args.workers <= 4 and 0 < args.timeout <= 60
    with ThreadPoolExecutor(max_workers=args.workers) as executor:
        rows = list(executor.map(lambda item: check(item, args.timeout), items))
    result = dict(planned=len(items), tested=len(rows), results=rows,
                  note='Bounded HTTPS reachability only; not a playback pass.')
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    for row in rows:
        print(json.dumps(dict(id=row['id'], title=row['title'], status=row['status'],
                              http=[request.get('status', request.get('error_type')) for request in row['requests']])))


if __name__ == '__main__':
    main()

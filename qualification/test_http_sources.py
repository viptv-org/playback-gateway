"""External preflight must bound reads and keep credential-bearing references private."""
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import urllib.request
sys.path.insert(0, str(Path(__file__).parent / 'media'))
from http_sources import check, Redirects


class Response(io.BytesIO):
    def __init__(self, status, body=b''):
        super().__init__(body)
        self.status = status
        self.url = 'https://cdn.example/private-credential'
        self.headers = {'Content-Type': 'video/mp4'}
        self.requested = []

    def read(self, size=-1):
        self.requested.append(size)
        if size < 0 or size > 65536:
            raise AssertionError('unbounded source read')
        return super().read(size)


class HTTPSourceTests(unittest.TestCase):
    def test_head_failure_does_not_hide_valid_get_and_body_read_is_bounded(self):
        head = Response(400)
        get = Response(206, b'\0\0\0\x20ftyp' + b'x' * 100000)
        with patch('http_sources.urllib.request.build_opener') as build:
            build.return_value.open.side_effect = [head, get]
            result = check({'id': 'ref', 'url': 'https://source.example/private-credential'}, 1)
        self.assertEqual(result['status'], 'reachable_media')
        self.assertEqual(get.requested, [65536])
        self.assertNotIn('private-credential', json.dumps(result))
        self.assertTrue(head.closed and get.closed)

    def test_exception_text_cannot_leak_source_credentials(self):
        with patch('http_sources.urllib.request.build_opener') as build:
            build.return_value.open.side_effect = OSError('https://example/SECRET')
            result = check({'id': 'ref', 'url': 'https://example/SECRET'}, 1)
        self.assertEqual(result['status'], 'upstream_failed')
        self.assertNotIn('SECRET', json.dumps(result))

    def test_successful_html_response_is_not_media(self):
        with patch('http_sources.urllib.request.build_opener') as build:
            build.return_value.open.side_effect = [Response(200), Response(200, b'<html>login</html>')]
            result = check({'id': 'ref', 'url': 'https://example/media'}, 1)
        self.assertEqual(result['status'], 'upstream_failed')

    def test_redirects_reject_downgrade_and_strip_credentials_across_hosts(self):
        req = urllib.request.Request('https://first.example/media', headers={
            'Authorization': 'Bearer SECRET', 'Cookie': 'SECRET', 'Range': 'bytes=0-65535'})
        redirects = Redirects()
        with self.assertRaises(ValueError):
            redirects.redirect_request(req, None, 302, '', {}, 'http://second.example/media')
        redirected = redirects.redirect_request(req, None, 302, '', {}, 'https://second.example/media')
        self.assertIsNone(redirected.get_header('Authorization'))
        self.assertIsNone(redirected.get_header('Cookie'))
        self.assertEqual(redirected.get_header('Range'), 'bytes=0-65535')


if __name__ == '__main__':
    unittest.main()

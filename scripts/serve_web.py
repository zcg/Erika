#!/usr/bin/env python3
"""Serve the built browser example on localhost, including HTTP byte ranges."""
import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import re

class Handler(SimpleHTTPRequestHandler):
    def send_head(self):
        self.range_end = None
        header = self.headers.get('Range')
        path = self.translate_path(self.path)
        if not header or not os.path.isfile(path):
            return super().send_head()
        match = re.fullmatch(r'bytes=(\d+)-(\d*)', header)
        size = os.path.getsize(path)
        if not match:
            self.send_error(416); return None
        start = int(match[1]); end = min(size-1, int(match[2]) if match[2] else size-1)
        if start > end:
            self.send_error(416); return None
        file = open(path, 'rb'); file.seek(start)
        self.range_end = end-start+1
        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(path))
        self.send_header('Content-Length', str(self.range_end))
        self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
        self.send_header('Accept-Ranges', 'bytes')
        self.end_headers()
        return file
    def copyfile(self, source, output):
        if self.range_end is None:
            return super().copyfile(source, output)
        remaining = self.range_end
        try:
            while remaining:
                data = source.read(min(65536, remaining))
                if not data: break
                output.write(data); remaining -= len(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port',type=int,default=18765)
    parser.add_argument('--directory',default=str(Path(__file__).resolve().parents[1]/'target/web'))
    args=parser.parse_args()
    server=ThreadingHTTPServer(('127.0.0.1',args.port),partial(Handler,directory=args.directory))
    print(f'http://127.0.0.1:{server.server_port}',flush=True)
    server.serve_forever()

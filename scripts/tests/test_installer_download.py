import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer


class InstallerDownloadTests(unittest.TestCase):
    def test_reset_restarts_download_without_retaining_partial_archive(self):
        archive = b'verified archive contents'
        attempts = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                attempts.append(self.path)
                self.send_response(200)
                self.send_header('Content-Length', str(len(archive)))
                self.end_headers()
                if len(attempts) == 1:
                    self.wfile.write(b'partial')
                    self.wfile.flush()
                    self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
                else:
                    self.wfile.write(archive)

            def log_message(self, *args):
                pass

        source = (Path(__file__).resolve().parents[1] / 'install-tako-server.sh').read_text()
        functions = source[source.index('github_auth_header() {'):source.index('download_stdout() {')]
        with HTTPServer(('127.0.0.1', 0), Handler) as server, tempfile.TemporaryDirectory() as tmp:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                dest = Path(tmp) / 'archive'
                url = f'http://127.0.0.1:{server.server_port}/archive'
                subprocess.run(['sh', '-ec', 'need_cmd() { command -v "$1" >/dev/null; }\n'
                                + functions + '\ndownload_file "$1" "$2"', 'sh', url, str(dest)],
                               env={**os.environ, 'no_proxy': '127.0.0.1'}, check=True,
                               capture_output=True, timeout=15)
                self.assertEqual(attempts, ['/archive', '/archive'])
                self.assertEqual(dest.read_bytes(), archive)
            finally:
                server.shutdown()
                thread.join()

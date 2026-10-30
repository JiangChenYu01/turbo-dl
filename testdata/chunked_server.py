"""chunked / 无 Content-Length 流式测试服务器。"""
import os
import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            self.send_error(404)
            return
        size = os.path.getsize(path)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        with open(path, "rb") as f:
            while True:
                data = f.read(65536)
                if not data:
                    break
                self.wfile.write(b"%x\r\n" % len(data))
                self.wfile.write(data)
                self.wfile.write(b"\r\n")
        self.wfile.write(b"0\r\n\r\n")

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8768
    os.chdir(os.path.dirname(os.path.abspath(__file__)))
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
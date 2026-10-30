"""支持 HTTP Range 分段的本地测试服务器（用于 turbo-dl 功能自测）。"""
import http.server
import os
import re
import sys


class RangeHandler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send_head(self):
        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            self.send_error(404)
            return None
        size = os.path.getsize(path)
        range_header = self.headers.get("Range")
        self.range_start = 0
        self.range_end = size - 1
        status = 200
        if range_header:
            m = re.match(r"bytes=(\d*)-(\d*)", range_header.strip())
            if m:
                start = int(m.group(1) or 0)
                end = int(m.group(2) or (size - 1))
                if start < size:
                    end = min(end, size - 1)
                    self.range_start = start
                    self.range_end = end
                    status = 206
        self.range_size = self.range_end - self.range_start + 1
        self.send_response(status)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Content-Length", str(self.range_size))
        if status == 206:
            self.send_header("Content-Range", f"bytes {self.range_start}-{self.range_end}/{size}")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Last-Modified", self.date_time_string())
        self.end_headers()
        return open(path, "rb")

    def do_GET(self):
        f = self.send_head()
        if f:
            try:
                f.seek(self.range_start)
                remaining = self.range_size
                while remaining > 0:
                    data = f.read(min(65536, remaining))
                    if not data:
                        break
                    self.wfile.write(data)
                    remaining -= len(data)
            finally:
                f.close()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8766
    http.server.test(HandlerClass=RangeHandler, port=port, bind="127.0.0.1")
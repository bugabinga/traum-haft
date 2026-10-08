# Test app for werk: echoes who is asking, what it got, and a counter in /data.
import json, os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = os.environ.get("APP_VERSION", "1")


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz" and os.environ.get("BROKEN"):
            self.send_response(500); self.end_headers(); return
        counter = "/data/counter"
        n = int(open(counter).read()) + 1 if os.path.exists(counter) else 1
        open(counter, "w").write(str(n))
        print(f"request {self.path} from {self.headers.get('X-User-Email')}", flush=True)
        body = json.dumps({
            "version": VERSION,
            "email": self.headers.get("X-User-Email"),
            "sub": self.headers.get("X-User-Sub"),
            "cookie": self.headers.get("Cookie"),
            "edge_header": self.headers.get("X-Traum-Haft-Edge"),
            "secret": os.environ.get("DEMO_SECRET"),
            "app": os.environ.get("TRAUM_HAFT_APP"),
            "url": os.environ.get("TRAUM_HAFT_URL"),
            "counter": n,
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


ThreadingHTTPServer(("0.0.0.0", int(os.environ["PORT"])), H).serve_forever()

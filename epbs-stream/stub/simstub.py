"""Stub simulator for a helix relay: answers the SSZ /validate route with 200 (an empty body
means 'valid' to helix) and the JSON-RPC health probes (eth_syncing -> false, eth_getBalance ->
a large balance). Logs one line per request so the test can show the relay simulated each bid."""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = self.rfile.read(n)
        if self.path.startswith("/validate"):
            print(f"{time.time():.3f} simstub {self.path} bytes={n} -> 200", flush=True)
            self.send_response(200)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        try:
            req = json.loads(body or b"{}")
        except Exception:
            req = {}
        method = req.get("method")
        if method == "eth_syncing":
            result = False
        elif method == "eth_getBalance":
            result = hex(10**24)
        else:
            result = None
        out = json.dumps({"jsonrpc": "2.0", "id": req.get("id", 1), "result": result}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8545
    print(f"simstub listening on {port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()

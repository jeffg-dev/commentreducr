"""Stub /v1/systemone: uniform probabilities for every question. For tests."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer


def answer(q):
    if q["type"] == "noul":
        return {"type": "noul", "noul": 0.5}
    opts = q["criteria"] if q["type"] == "choice" else [str(i) for i in range(len(q["criteria"]))]
    opts = list(opts)
    p = {o: 1 / len(opts) for o in opts}
    return {"type": q["type"], "probabilities": p, **({"score": 0.0} if q["type"] == "score" else {})}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        out = json.dumps({"answers": {k: answer(q) for k, q in body["questions"].items()},
                          "usage": {"input_tokens": len(body["state"]) // 4}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *a):
        pass


def serve(port=0):
    """Start in a thread; returns (server, url)."""
    import threading
    s = HTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=s.serve_forever, daemon=True).start()
    return s, f"http://127.0.0.1:{s.server_port}/v1/systemone"


if __name__ == "__main__":
    s, url = serve(8100)
    print(url)
    s.serve_forever()

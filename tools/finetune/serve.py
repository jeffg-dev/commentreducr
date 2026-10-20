#!/usr/bin/env python3
"""Serve a fine-tuned Clef-Flash on POST /v1/systemone (same body and response as the zero-shot server).

usage: serve.py [--ckpt ~/ft/flash-ft/best] [--port 8102] [--model <clef-flash snapshot>]
"""
import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import torch

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from train import DEFAULT_MODEL, load  # noqa: E402

ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument("--model", default=DEFAULT_MODEL)
ap.add_argument("--ckpt", default=os.path.expanduser("~/ft/flash-ft/best"))
ap.add_argument("--port", type=int, default=8102)
a = ap.parse_args()
model, processor = load(a.model, ckpt=a.ckpt)
from joint_schema_model import systemone  # noqa: E402  (on sys.path after load)

lock = threading.Lock()


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/systemone":
            return self.send_error(404)
        try:
            req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            t = time.time()
            with lock, torch.autocast("cuda", dtype=torch.bfloat16):
                resp = systemone(model, processor, req)
        except (ValueError, KeyError, TypeError) as e:
            return self.send_error(400, str(e))
        body = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        print(f"{resp['usage']['input_tokens']} tok {time.time() - t:.2f}s", flush=True)

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", a.port), H).serve_forever()

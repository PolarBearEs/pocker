#!/usr/bin/env python3

import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def compact_json(value):
    return json.dumps(value, separators=(",", ":")).encode()


layer = b"pocker smoke layer\n"
layer_digest = digest(layer)
config = compact_json({"rootfs": {"diff_ids": [layer_digest]}})
config_digest = digest(config)
manifest = compact_json(
    {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": len(config),
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": layer_digest,
                "size": len(layer),
            }
        ],
    }
)
manifest_digest = digest(manifest)
blobs = {
    config_digest: ("application/vnd.oci.image.config.v1+json", config),
    layer_digest: ("application/vnd.oci.image.layer.v1.tar", layer),
}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/v2/":
            self.respond(200, "text/plain", b"")
            return
        if self.path == "/v2/sample/manifests/latest":
            self.respond(
                200,
                "application/vnd.oci.image.manifest.v1+json",
                manifest,
                {"Docker-Content-Digest": manifest_digest},
            )
            return
        prefix = "/v2/sample/blobs/"
        if self.path.startswith(prefix):
            blob_digest = self.path[len(prefix) :]
            if blob_digest in blobs:
                media_type, body = blobs[blob_digest]
                self.respond(
                    200,
                    media_type,
                    body,
                    {"Docker-Content-Digest": blob_digest},
                )
                return
        self.respond(404, "text/plain", b"")

    def do_HEAD(self):
        prefix = "/v2/sample/blobs/"
        if self.path.startswith(prefix):
            blob_digest = self.path[len(prefix) :]
            if blob_digest in blobs:
                media_type, body = blobs[blob_digest]
                self.respond(
                    200,
                    media_type,
                    b"",
                    {
                        "Content-Length": str(len(body)),
                        "Docker-Content-Digest": blob_digest,
                    },
                )
                return
        self.respond(404, "text/plain", b"")

    def respond(self, status, media_type, body, headers=None):
        headers = headers or {}
        self.send_response(status)
        self.send_header("Content-Type", media_type)
        if "Content-Length" not in headers:
            self.send_header("Content-Length", str(len(body)))
        self.send_header("Docker-Distribution-API-Version", "registry/2.0")
        for name, value in headers.items():
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def log_message(self, fmt, *args):
        pass


def main():
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} PORT", file=sys.stderr)
        return 2
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

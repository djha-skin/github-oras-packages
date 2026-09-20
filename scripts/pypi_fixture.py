#!/usr/bin/env python3
"""Loopback-only OCI Distribution fixture for the PyPI MVP smoke command."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


AUTOINDEX_REFERENCE = "autoindex.v1"
AUTOINDEX_CONFIG = b"{}"
AUTOINDEX_CONFIG_DIGEST = "sha256:" + hashlib.sha256(AUTOINDEX_CONFIG).hexdigest()
VISIBILITY_ANNOTATION = "io.github.djha-skin.github-oras-packages.autoindex.visible"


def autoindex_manifest(corpus: pathlib.Path) -> bytes:
    routes = json.loads((corpus / "route-map.json").read_text(encoding="utf-8"))["routes"]
    layers = []
    for route in routes:
        if route["protocol"] != "pypi":
            continue
        descriptor = route["descriptor"]
        title = route["path"].rstrip("/") + "/index.html" if route["path"].endswith("/") else route["path"]
        layers.append(
            {
                "mediaType": descriptor["mediaType"],
                "digest": descriptor["digest"],
                "size": descriptor["size"],
                "annotations": {
                    "org.opencontainers.image.title": title,
                    VISIBILITY_ANNOTATION: "true",
                },
            }
        )
    return json.dumps(
        {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": AUTOINDEX_CONFIG_DIGEST,
                "size": len(AUTOINDEX_CONFIG),
            },
            "layers": layers,
        },
        separators=(",", ":"),
    ).encode("ascii")


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        self._serve(send_body=True)

    def do_HEAD(self) -> None:  # noqa: N802 - stdlib handler API
        self._serve(send_body=False)

    def _serve(self, *, send_body: bool) -> None:
        server = self.server
        assert isinstance(server, FixtureServer)
        path = self.path.split("?", 1)[0]
        with server.observations.open("a", encoding="ascii") as output:
            output.write(f"{self.command} {path}\n")

        if not path.startswith("/v2/acme/fixture/"):
            self._send(404, b"", "text/plain")
            return

        corpus = server.corpus
        if path == "/v2/acme/fixture/manifests/oras-packages.v1":
            body = (corpus / "manifest.json").read_bytes()
            content_type = "application/vnd.oci.image.manifest.v1+json"
        elif path == "/v2/acme/fixture/manifests/autoindex.v1" and server.autoindex:
            body = autoindex_manifest(corpus)
            content_type = "application/vnd.oci.image.manifest.v1+json"
        elif path == "/v2/acme/fixture/blobs/" + AUTOINDEX_CONFIG_DIGEST and server.autoindex:
            body = AUTOINDEX_CONFIG
            content_type = "application/vnd.oci.empty.v1+json"
        elif path.startswith("/v2/acme/fixture/blobs/sha256:"):
            digest = path.rsplit("/", 1)[-1].removeprefix("sha256:")
            if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
                self._send(404, b"", "text/plain")
                return
            blob = corpus / "blobs" / "sha256" / digest
            if not blob.is_file():
                self._send(404, b"", "text/plain")
                return
            body = blob.read_bytes()
            content_type = "application/octet-stream"
        else:
            self._send(404, b"", "text/plain")
            return

        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        if send_body:
            self.wfile.write(body)

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class FixtureServer(ThreadingHTTPServer):
    allow_reuse_address = False
    daemon_threads = True

    def __init__(
        self, corpus: pathlib.Path, observations: pathlib.Path, autoindex: bool
    ) -> None:
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.corpus = corpus
        self.observations = observations
        self.autoindex = autoindex


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--ready-file", type=pathlib.Path, required=True)
    parser.add_argument("--observations", type=pathlib.Path, required=True)
    parser.add_argument("--autoindex", action="store_true")
    args = parser.parse_args()
    args.observations.write_text("", encoding="ascii")
    server = FixtureServer(args.corpus, args.observations, args.autoindex)
    args.ready_file.write_text(str(server.server_port), encoding="ascii")
    try:
        server.serve_forever(poll_interval=0.05)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

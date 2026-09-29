#!/usr/bin/env python3
"""Loopback-only in-memory OCI Distribution registry for ORAS smoke tests."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit


REPOSITORY = "acme/fixture"
PREFIX = f"/v2/{REPOSITORY}"
MAX_BODY_BYTES = 64 * 1024 * 1024


def digest_bytes(content: bytes) -> str:
    return "sha256:" + hashlib.sha256(content).hexdigest()


class RegistryState:
    def __init__(self, observations: pathlib.Path) -> None:
        self.observations = observations
        self.lock = threading.Lock()
        self.blobs: dict[str, tuple[bytes, str]] = {}
        self.manifests: dict[str, bytes] = {}
        self.uploads: dict[str, bytearray] = {}
        self.upload_states: dict[str, str] = {}

    def observe(self, method: str, path: str) -> None:
        # Never persist query values: upload-state parameters are bearer-like.
        with self.lock, self.observations.open("a", encoding="ascii") as output:
            output.write(f"{method} {path}\n")


class RegistryHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    @property
    def state(self) -> RegistryState:
        server = self.server
        assert isinstance(server, RegistryServer)
        return server.state

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=True)

    def do_HEAD(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=False)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=True)

    def do_PATCH(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=True)

    def do_PUT(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=True)

    def do_DELETE(self) -> None:  # noqa: N802 - stdlib handler API
        self._dispatch(send_body=True)

    def _dispatch(self, *, send_body: bool) -> None:
        parsed = urlsplit(self.path)
        path = parsed.path
        self.state.observe(self.command, path)
        if path == "/v2/" and self.command in {"GET", "HEAD"}:
            self._send(200, b"", send_body=send_body,
                       headers={"Docker-Distribution-Api-Version": "registry/2.0"})
            return
        if path != PREFIX and not path.startswith(PREFIX + "/"):
            self._send(404, b"", send_body=send_body)
            return

        try:
            body = self._read_body()
        except (ValueError, OSError):
            self._send(400, b"", send_body=send_body)
            return

        if path.startswith(PREFIX + "/manifests/"):
            reference = path.removeprefix(PREFIX + "/manifests/")
            self._manifest(reference, body, send_body)
        elif path.startswith(PREFIX + "/blobs/uploads"):
            self._upload(path, parsed.query, body, send_body)
        elif path.startswith(PREFIX + "/blobs/"):
            digest = path.removeprefix(PREFIX + "/blobs/")
            self._blob(digest, body, send_body)
        else:
            self._send(404, b"", send_body=send_body)

    def _read_body(self) -> bytes:
        if self.headers.get("Transfer-Encoding") is not None:
            raise ValueError("transfer encoding is unsupported by the fixture")
        raw_length = self.headers.get("Content-Length", "0")
        if not raw_length.isascii() or not raw_length.isdigit():
            raise ValueError("invalid content length")
        length = int(raw_length)
        if length > MAX_BODY_BYTES:
            raise ValueError("request body exceeds fixture limit")
        body = self.rfile.read(length)
        if len(body) != length:
            raise OSError("truncated request body")
        return body

    def _manifest(self, reference: str, body: bytes, send_body: bool) -> None:
        if not reference or "/" in reference:
            self._send(404, b"", send_body=send_body)
            return
        if self.command == "PUT":
            try:
                manifest = json.loads(body)
                if manifest.get("schemaVersion") != 2:
                    raise ValueError
                verified_blobs = []
                for descriptor in [manifest["config"], *manifest["layers"]]:
                    digest = descriptor["digest"]
                    media_type = descriptor["mediaType"]
                    blob = self.state.blobs.get(digest)
                    if (
                        not isinstance(media_type, str)
                        or blob is None
                        or len(blob[0]) != descriptor["size"]
                    ):
                        raise ValueError
                    if digest_bytes(blob[0]) != digest:
                        raise ValueError
                    verified_blobs.append((digest, blob[0], media_type))
            except (KeyError, TypeError, ValueError, json.JSONDecodeError):
                self._send(400, b"", send_body=send_body)
                return
            digest = digest_bytes(body)
            with self.state.lock:
                for blob_digest, blob_content, media_type in verified_blobs:
                    self.state.blobs[blob_digest] = (blob_content, media_type)
                self.state.manifests[reference] = body
                self.state.manifests[digest] = body
            self._send(201, b"", send_body=send_body,
                       headers={"Docker-Content-Digest": digest})
            return
        if self.command == "DELETE":
            content = self.state.manifests.get(reference)
            if content is None:
                self._send(404, b"", send_body=send_body)
                return
            digest = digest_bytes(content)
            with self.state.lock:
                for name, candidate in list(self.state.manifests.items()):
                    if candidate == content:
                        del self.state.manifests[name]
            self._send(202, b"", send_body=send_body,
                       headers={"Docker-Content-Digest": digest})
            return
        if self.command not in {"GET", "HEAD"}:
            self._send(405, b"", send_body=send_body,
                       headers={"Allow": "GET, HEAD, PUT, DELETE"})
            return
        content = self.state.manifests.get(reference)
        if content is None:
            self._send(404, b"", send_body=send_body)
            return
        try:
            media_type = json.loads(content).get(
                "mediaType", "application/vnd.oci.image.manifest.v1+json"
            )
        except (json.JSONDecodeError, AttributeError):
            media_type = "application/vnd.oci.image.manifest.v1+json"
        self._send(
            200,
            content,
            send_body=send_body,
            headers={
                "Content-Type": media_type,
                "Docker-Content-Digest": digest_bytes(content),
                "ETag": f'"{digest_bytes(content)}"',
            },
        )

    def _blob(self, digest: str, body: bytes, send_body: bool) -> None:
        if self.command == "DELETE":
            with self.state.lock:
                existed = self.state.blobs.pop(digest, None)
            self._send(202 if existed is not None else 404, b"", send_body=send_body)
            return
        if self.command not in {"GET", "HEAD"} or body:
            self._send(405, b"", send_body=send_body,
                       headers={"Allow": "GET, HEAD, DELETE"})
            return
        blob = self.state.blobs.get(digest)
        if blob is None:
            self._send(404, b"", send_body=send_body)
            return
        content, media_type = blob
        self._send(
            200,
            content,
            send_body=send_body,
            headers={
                "Content-Type": media_type,
                "Docker-Content-Digest": digest,
                "ETag": f'"{digest}"',
            },
        )

    def _upload(self, path: str, query: str, body: bytes, send_body: bool) -> None:
        start_path = PREFIX + "/blobs/uploads"
        if path in {start_path, start_path + "/"} and self.command == "POST":
            if query:
                self._send(400, b"", send_body=send_body)
                return
            upload_id = str(uuid.uuid4())
            state = uuid.uuid4().hex
            with self.state.lock:
                self.state.uploads[upload_id] = bytearray()
                self.state.upload_states[upload_id] = state
            location = f"{start_path}/{upload_id}?_state={state}"
            self._send(
                202,
                b"",
                send_body=send_body,
                headers={
                    "Location": location,
                    "Docker-Upload-UUID": upload_id,
                    "Range": "0-0",
                },
            )
            return
        if not path.startswith(start_path + "/"):
            self._send(404, b"", send_body=send_body)
            return
        upload_id = path.removeprefix(start_path + "/")
        if not upload_id or "/" in upload_id:
            self._send(404, b"", send_body=send_body)
            return
        params = parse_qs(query, keep_blank_values=True, strict_parsing=False)
        with self.state.lock:
            expected_state = self.state.upload_states.get(upload_id)
            upload = self.state.uploads.get(upload_id)
            if upload is None or params.get("_state") != [expected_state]:
                self._send(404, b"", send_body=send_body)
                return
            if self.command == "PATCH":
                if params.keys() - {"_state"}:
                    self._send(400, b"", send_body=send_body)
                    return
                if len(upload) + len(body) > MAX_BODY_BYTES:
                    self._send(413, b"", send_body=send_body)
                    return
                upload.extend(body)
                location = f"{start_path}/{upload_id}?_state={expected_state}"
                self._send(
                    202,
                    b"",
                    send_body=send_body,
                    headers={
                        "Location": location,
                        "Docker-Upload-UUID": upload_id,
                        "Range": f"0-{max(len(upload) - 1, 0)}",
                    },
                )
                return
            if self.command == "PUT":
                digest_values = params.get("digest", [])
                if params.keys() - {"_state", "digest"} or len(digest_values) != 1:
                    self._send(400, b"", send_body=send_body)
                    return
                if len(upload) + len(body) > MAX_BODY_BYTES:
                    self._send(413, b"", send_body=send_body)
                    return
                upload.extend(body)
                digest = digest_bytes(bytes(upload))
                if digest != digest_values[0]:
                    self._send(400, b"", send_body=send_body)
                    return
                self.state.blobs[digest] = (bytes(upload), "application/octet-stream")
                del self.state.uploads[upload_id]
                del self.state.upload_states[upload_id]
                self._send(
                    201,
                    b"",
                    send_body=send_body,
                    headers={
                        "Location": f"{PREFIX}/blobs/{digest}",
                        "Docker-Content-Digest": digest,
                        "Range": f"0-{max(len(upload) - 1, 0)}",
                    },
                )
                return
            if self.command == "DELETE":
                self.state.uploads.pop(upload_id, None)
                self.state.upload_states.pop(upload_id, None)
                self._send(204, b"", send_body=send_body)
                return
        self._send(405, b"", send_body=send_body,
                   headers={"Allow": "PATCH, PUT, DELETE"})

    def _send(
        self,
        status: int,
        content: bytes,
        *,
        send_body: bool,
        headers: dict[str, str] | None = None,
    ) -> None:
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        if send_body and content:
            self.wfile.write(content)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class RegistryServer(ThreadingHTTPServer):
    allow_reuse_address = False
    daemon_threads = True

    def __init__(self, observations: pathlib.Path) -> None:
        super().__init__(("127.0.0.1", 0), RegistryHandler)
        self.state = RegistryState(observations)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=pathlib.Path, required=True)
    parser.add_argument("--observations", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.observations.write_text("", encoding="ascii")
    server = RegistryServer(args.observations)
    args.ready_file.write_text(str(server.server_port), encoding="ascii")
    try:
        server.serve_forever(poll_interval=0.05)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

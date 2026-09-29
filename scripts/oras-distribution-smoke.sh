#!/bin/sh
# OCI Distribution and native autoindex publication black-box smoke.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=${TMPDIR:-/tmp}/oras-distribution-smoke.$$
FIXTURE_READY=$TMP/fixture.port
FIXTURE_OBSERVATIONS=$TMP/observations
APP_DATA_DIR=$TMP/app-data
FIXTURE_PID=
PROXY_PID=

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ -n "$PROXY_PID" ]; then kill "$PROXY_PID" 2>/dev/null || true; fi
    if [ -n "$FIXTURE_PID" ]; then kill "$FIXTURE_PID" 2>/dev/null || true; fi
    wait "$PROXY_PID" 2>/dev/null || true
    wait "$FIXTURE_PID" 2>/dev/null || true
    rm -rf "$TMP"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

command -v oras >/dev/null 2>&1 || {
    echo "oras CLI is required for this smoke test" >&2
    exit 1
}
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace --release
mkdir -p "$TMP" "$APP_DATA_DIR"

python3 "$ROOT/scripts/oci_registry_stateful.py" \
    --ready-file "$FIXTURE_READY" \
    --observations "$FIXTURE_OBSERVATIONS" &
FIXTURE_PID=$!
for _ in $(seq 1 100); do
    [ -s "$FIXTURE_READY" ] && break
    sleep 0.01
done
[ -s "$FIXTURE_READY" ]
FIXTURE_PORT=$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).read_text())' "$FIXTURE_READY")

PROXY_PORT=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
start_proxy() {
    (
        cd "$APP_DATA_DIR"
        exec env \
            ORAS_PROXY_LISTEN_ADDR="127.0.0.1:$PROXY_PORT" \
            ORAS_PROXY_UPSTREAM="http://127.0.0.1:$FIXTURE_PORT" \
            ORAS_PROXY_REPOSITORY=acme/fixture \
            ORAS_PROXY_ALLOWED_HOSTS=127.0.0.1 \
            ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true \
            "$ROOT/target/release/github-oras-packages-proxy" serve
    ) &
    PROXY_PID=$!
    for _ in $(seq 1 100); do
        if python3 - "$PROXY_PORT" <<'PY'
import socket
import sys
try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.1) as sock:
        sock.sendall(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        if sock.recv(128).startswith(b"HTTP/1.1 200 OK"):
            raise SystemExit(0)
except OSError:
    pass
raise SystemExit(1)
PY
        then return; fi
        kill -0 "$PROXY_PID" 2>/dev/null || exit 1
        sleep 0.01
    done
    exit 1
}

stop_proxy() {
    if [ -n "$PROXY_PID" ]; then
        kill "$PROXY_PID" 2>/dev/null || true
        wait "$PROXY_PID" 2>/dev/null || true
        PROXY_PID=
    fi
}

start_proxy

REGISTRY="127.0.0.1:$PROXY_PORT/acme/fixture"
TARGET="$REGISTRY:oras-smoke"
printf 'hello from ORAS\n' >"$TMP/hello.txt"
(
    cd "$TMP"
    oras push --plain-http --no-tty "$TARGET" hello.txt:text/plain
)
oras manifest fetch --plain-http "$TARGET" --output "$TMP/manifest.json"
mkdir "$TMP/pulled"
oras pull --plain-http --no-tty --output "$TMP/pulled" "$TARGET"
python3 - "$TMP" <<'PY'
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "manifest.json").read_text())
assert manifest["schemaVersion"] == 2
assert (root / "pulled/hello.txt").read_bytes() == b"hello from ORAS\n"
assert any(layer.get("annotations", {}).get("org.opencontainers.image.title") == "hello.txt"
           for layer in manifest["layers"])
PY
oras manifest delete --plain-http --force "$TARGET"
if oras manifest fetch --plain-http "$TARGET" >/dev/null 2>&1; then
    echo "manifest unexpectedly remained after delete" >&2
    exit 1
fi
python3 - "$PROXY_PORT" "$TMP/manifest.json" <<'PY'
import http.client
import json
import pathlib
import sys
port = int(sys.argv[1])
manifest = json.loads(pathlib.Path(sys.argv[2]).read_text())
connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
connection.request("POST", "/v2/acme/fixture/blobs/uploads/", body=b"")
response = connection.getresponse()
assert response.status == 202
location = response.getheader("Location")
assert location and location.startswith("/v2/acme/fixture/blobs/uploads/")
response.read()
connection.request("DELETE", location, body=b"")
response = connection.getresponse()
assert response.status == 204
response.read()
for descriptor in [manifest["config"], *manifest["layers"]]:
    path = f"/v2/acme/fixture/blobs/{descriptor['digest']}"
    connection.request("DELETE", path, body=b"")
    response = connection.getresponse()
    assert response.status == 202, (path, response.status)
    response.read()
    connection.request("HEAD", path)
    response = connection.getresponse()
    assert response.status == 404, (path, response.status)
    response.read()
connection.close()
PY

mkdir -p "$TMP/native/simple/demo" "$TMP/native/packages"
printf '<a href="demo/">demo</a>\n' >"$TMP/native/simple/index.html"
printf '<a href="../../packages/demo-1.0-py3-none-any.whl">demo</a>\n' >"$TMP/native/simple/demo/index.html"
printf 'fixture wheel bytes\n' >"$TMP/native/packages/demo-1.0-py3-none-any.whl"
"$ROOT/target/release/autoindex-publish" \
    "$TMP/native" "$TMP/autoindex-layout" autoindex.v1
oras cp --from-oci-layout --to-plain-http --no-tty \
    "$TMP/autoindex-layout:autoindex.v1" "$REGISTRY:autoindex.v1"
oras manifest fetch --plain-http "$REGISTRY:autoindex.v1" \
    --output "$TMP/autoindex-manifest.json"
mkdir "$TMP/autoindex-pulled"
oras pull --plain-http --no-tty --output "$TMP/autoindex-pulled" \
    "$REGISTRY:autoindex.v1"
python3 - "$TMP" "$PROXY_PORT" <<'PY'
import http.client
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "autoindex-manifest.json").read_text())
assert (root / "autoindex-pulled/simple/demo/index.html").is_file()
assert (root / "autoindex-pulled/packages/demo-1.0-py3-none-any.whl").read_bytes() == b"fixture wheel bytes\n"
assert all(layer.get("annotations", {}).get("io.github.djha-skin.github-oras-packages.autoindex.visible") == "true"
           for layer in manifest["layers"])
connection = http.client.HTTPConnection("127.0.0.1", int(sys.argv[2]), timeout=5)
for path, expected in [
    ("/", b"simple/"),
    ("/simple/", b"demo/"),
    ("/simple/demo/index.html", b"../../packages/demo-1.0-py3-none-any.whl"),
    ("/packages/demo-1.0-py3-none-any.whl", b"fixture wheel bytes"),
]:
    connection.request("GET", path)
    response = connection.getresponse()
    body = response.read()
    assert response.status == 200 and expected in body, (path, response.status)
connection.request("HEAD", "/packages/demo-1.0-py3-none-any.whl")
response = connection.getresponse()
assert response.status == 200 and response.getheader("Content-Length") == str(len(b"fixture wheel bytes\n"))
response.read()
connection.request("GET", "/not-found")
response = connection.getresponse()
assert response.status == 404
response.read()
connection.close()
PY

stop_proxy
python3 - "$APP_DATA_DIR" <<'PY'
import pathlib
import sys
assert not [path for path in pathlib.Path(sys.argv[1]).rglob("*") if path.is_file()]
PY
start_proxy
python3 - "$APP_DATA_DIR" "$PROXY_PORT" <<'PY'
import http.client
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
connection = http.client.HTTPConnection("127.0.0.1", int(sys.argv[2]), timeout=5)
connection.request("GET", "/simple/demo/index.html")
response = connection.getresponse()
assert response.status == 200
assert b"../../packages/demo-1.0-py3-none-any.whl" in response.read()
connection.close()
assert not [path for path in root.rglob("*") if path.is_file()]
PY

python3 - "$FIXTURE_OBSERVATIONS" <<'PY'
import pathlib
import sys
lines = pathlib.Path(sys.argv[1]).read_text().splitlines()
assert all(line.split(" ", 1)[1].startswith("/v2/acme/fixture/") for line in lines), lines
methods = {line.split(" ", 1)[0] for line in lines}
assert {"POST", "PUT", "GET", "HEAD", "DELETE"} <= methods, methods
PY

echo "ORAS OCI Distribution push/fetch/pull/delete smoke test passed"

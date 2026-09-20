#!/bin/sh
# Hermetic standard OCI autoindex smoke test: fixture -> serve -> natural paths.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=${TMPDIR:-/tmp}/oras-proxy-autoindex-smoke.$$
FIXTURE_READY=$TMP/fixture.port
FIXTURE_OBSERVATIONS=$TMP/observations
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
mkdir -p "$TMP"

python3 "$ROOT/scripts/pypi_fixture.py" \
    --corpus "$ROOT/fixtures/corpus" \
    --autoindex \
    --ready-file "$FIXTURE_READY" \
    --observations "$FIXTURE_OBSERVATIONS" &
FIXTURE_PID=$!
for _ in $(seq 1 100); do
    [ -s "$FIXTURE_READY" ] && break
    sleep 0.01
done
[ -s "$FIXTURE_READY" ]
FIXTURE_PORT=$(cat "$FIXTURE_READY")

PROXY_PORT=$(python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
ORAS_PROXY_LISTEN_ADDR="127.0.0.1:$PROXY_PORT" \
ORAS_PROXY_UPSTREAM="http://127.0.0.1:$FIXTURE_PORT" \
ORAS_PROXY_REPOSITORY=acme/fixture \
ORAS_PROXY_ALLOWED_HOSTS=127.0.0.1 \
ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true \
    "$ROOT/target/release/github-oras-packages-proxy" serve &
PROXY_PID=$!

for _ in $(seq 1 100); do
    if python3 - "$PROXY_PORT" <<'PY'
import socket
import sys
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.1) as sock:
    sock.sendall(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    data = sock.recv(128)
if data.startswith(b"HTTP/1.1 200 OK"):
    raise SystemExit(0)
raise SystemExit(1)
PY
    then break; fi
    kill -0 "$PROXY_PID" 2>/dev/null || exit 1
    sleep 0.01
done

python3 - "$PROXY_PORT" <<'PY'
import http.client
import sys

port = int(sys.argv[1])
def get(path, method="GET"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request(method, path)
    response = connection.getresponse()
    body = response.read()
    headers = {key.lower(): value for key, value in response.getheaders()}
    connection.close()
    return response.status, headers, body

status, headers, body = get("/")
assert status == 200 and b"/simple/" in body
assert headers["content-type"].startswith("text/html")
status, _, body = get("/simple/")
assert status == 200 and b"capturepkg" in body
status, headers, body = get("/simple/capturepkg/index.html")
assert status == 200 and b"capturepkg" in body
status, headers, body = get("/packages/capturepkg-1.0.0-py3-none-any.whl")
assert status == 200 and body.startswith(b"PK")
assert headers["content-type"] == "application/octet-stream"
status, headers, body = get("/packages/capturepkg-1.0.0-py3-none-any.whl", "HEAD")
assert status == 200 and not body and headers["content-length"] == "981"
status, headers, _ = get("/simple")
assert status == 308 and headers["location"] == "/simple/"
status, _, body = get("/does-not-exist")
assert status == 404 and b'"error":"not_found"' in body
status, _, body = get("/../does-not-exist")
assert status == 404 and b'"error":"not_found"' in body
PY

if grep -Ev '^(GET|HEAD) /v2/acme/fixture/' "$FIXTURE_OBSERVATIONS" | grep . >/dev/null 2>&1; then
    echo "fixture observed an unexpected repository path" >&2
    exit 1
fi
if grep -E '/r/v1/|YWNtZS9maXh0dXJl' "$FIXTURE_OBSERVATIONS" >/dev/null 2>&1; then
    echo "fixture observed a legacy encoded route" >&2
    exit 1
fi

echo "Standard OCI autoindex smoke test passed"

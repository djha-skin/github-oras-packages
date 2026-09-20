#!/bin/sh
# Hermetic PyPI MVP smoke test: local OCI fixture -> compiled proxy -> pip.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP=${TMPDIR:-/tmp}/oras-proxy-pypi-smoke.$$
FIXTURE_READY=$TMP/fixture.port
FIXTURE_OBSERVATIONS=$TMP/observations
VENV=$TMP/venv
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
    --ready-file "$FIXTURE_READY" \
    --observations "$FIXTURE_OBSERVATIONS" &
FIXTURE_PID=$!
for _ in $(seq 1 100); do
    [ -s "$FIXTURE_READY" ] && break
    sleep 0.01
done
[ -s "$FIXTURE_READY" ]
FIXTURE_PORT=$(cat "$FIXTURE_READY")

# Pick an unused loopback port for the compiled proxy, then wait for its
# health endpoint. Both ports are allocated locally and no public network is
# contacted.
PROXY_PORT=$(python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)
ORAS_PROXY_LISTEN_ADDR="127.0.0.1:$PROXY_PORT" \
ORAS_PROXY_UPSTREAM="http://127.0.0.1:$FIXTURE_PORT" \
ORAS_PROXY_ALLOWED_HOSTS=127.0.0.1 \
ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true \
ORAS_PROXY_ENABLED_FRONTENDS=pypi \
    "$ROOT/target/release/github-oras-packages-proxy" &
PROXY_PID=$!

for _ in $(seq 1 100); do
    if python3 - "$PROXY_PORT" <<'PY'
import socket
import sys
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.1) as sock:
    sock.sendall(b"GET /healthz HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n")
    data = sock.recv(128)
if data.startswith(b"HTTP/1.1 200 OK"):
    raise SystemExit(0)
raise SystemExit(1)
PY
    then break; fi
    kill -0 "$PROXY_PID" 2>/dev/null || exit 1
    sleep 0.01
done

python3 -m venv "$VENV"
PIP_DISABLE_PIP_VERSION_CHECK=1 "$VENV/bin/python" -m pip install \
    --no-deps --no-cache-dir \
    --index-url "http://127.0.0.1:$PROXY_PORT/r/v1/YWNtZS9maXh0dXJl/pypi/simple/" \
    capturepkg
"$VENV/bin/python" -c 'import capturepkg; assert capturepkg.VALUE == "fixture"'

# Unknown projects fail with the safe local 404 shape and never cause a
# cross-repository fixture request.
python3 - "$PROXY_PORT" <<'PY'
import http.client
import sys
connection = http.client.HTTPConnection("127.0.0.1", int(sys.argv[1]), timeout=5)
connection.request("GET", "/r/v1/YWNtZS9maXh0dXJl/pypi/simple/unknown/")
response = connection.getresponse()
body = response.read().decode("utf-8")
assert response.status == 404
assert '"error":"not_found"' in body
assert "acme/fixture" not in body
PY

# Ensure every fixture request stayed below the exact configured repository.
if grep -Ev '^(GET|HEAD) /v2/acme/fixture/' "$FIXTURE_OBSERVATIONS" | grep . >/dev/null 2>&1; then
    echo "fixture observed an unexpected repository path" >&2
    exit 1
fi

echo "PyPI MVP fixture smoke test passed"

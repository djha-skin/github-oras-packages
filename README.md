# GitHub ORAS Packages Proxy

A small Rust reverse proxy that exposes selected package-manager repository
paths from immutable blobs published in a GitHub Container Registry (GHCR) OCI
repository. The target package protocols are PyPI, RPM/DNF, Debian/APT, and
Arch/pacman.

The proxy is designed for constrained hosts such as a Raspberry Pi. It uses
bounded, backpressured streaming rather than buffering package artifacts, and
contacts only the configured GHCR-compatible origin. The first runnable
vertical slice is an anonymous/fixture-backed PyPI Simple API and wheel route;
other package protocols and production authentication remain separately
scoped.

## Development prerequisites

The pinned minimum supported Rust version (MSRV) is **Rust 1.88.0**. The
repository contains `rust-toolchain.toml`, so Rustup selects that toolchain
when it is installed:

```sh
rustup toolchain install 1.88.0 --profile minimal --component cargo --component clippy --component rustfmt
```

The workspace uses the Rust 2024 edition and Cargo resolver version 3.

## Local PyPI MVP

The fixture-backed PyPI vertical slice is exercised by the integration suite:

```sh
cargo test --test pypi_mvp
./scripts/pypi-mvp-smoke.sh
```

The shell command starts the checked-in Python OCI fixture and release proxy
on loopback ephemeral ports, runs `pip install --no-deps` in a temporary
virtual environment, imports `capturepkg`, checks an unknown-project failure,
and removes every process and temporary path on exit. It requires only Python,
pip, and the already-built release binary (`cargo build --workspace --release`).

It binds both the programmable OCI Distribution fixture and the proxy to
loopback ephemeral ports, resolves only `oras-packages.v1` in the exact
repository decoded from the public locator, serves the prebuilt Simple HTML
and wheel bytes, forwards an optional caller `Authorization` header only to
that fixed origin, and verifies safe unknown-project/missing-blob failures.
No external network or credential is required. The checked-in fixture corpus
is regenerated and verified with `python3 fixtures/generate.py` and
`python3 fixtures/verify.py`.

## Build and quality gates

From the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
python3 fixtures/verify.py
```

The checked-in fixture corpus under `fixtures/corpus` can be regenerated with
`python3 fixtures/generate.py`. It contains deterministic wheel, sdist, RPM,
DEB, pacman package/database, native metadata, OCI manifest/config/route-map,
and malformed resolver inputs. The fixture baseline is intentionally unsigned;
`fixtures/README.md` documents its signature policy and licensing.

## Dependency choices

The implementation deliberately uses a small, explicitly featured HTTP stack:

| Dependency | Purpose | Why it is suitable here |
| --- | --- | --- |
| [`tokio`](https://tokio.rs/) | Async runtime, TCP, signals | Mature non-blocking runtime; only macros, network, multi-thread runtime, and signal features are enabled. |
| [`hyper`](https://hyper.rs/) | HTTP/1 server and later HTTP bodies | Low-level HTTP primitives support streaming bodies without forcing a web framework or middleware stack. |
| [`hyper-util`](https://docs.rs/hyper-util/) | Tokio integration and server utilities | Bridges Hyper's runtime-agnostic primitives to Tokio, with only server/service/Tokio features enabled. |
| [`http-body-util`](https://docs.rs/http-body-util/) | HTTP body adapters | Provides narrow body combinators needed by the listener and response layer. |
| [`bytes`](https://docs.rs/bytes/) | Reference-counted byte buffers | Supports efficient byte chunks in the streaming proxy path. |
| [`base64`](https://docs.rs/base64/) | Repository-locator codec | Decodes and re-encodes the canonical unpadded base64url locator without adding a URL router. |
| [`serde`](https://serde.rs/) and [`serde_json`](https://serde.rs/) | OCI layout decoding | Decode only the strict bounded manifest/config/route-map contract. |
| [`sha2`](https://docs.rs/sha2/) | Content verification | Verify descriptor-bound SHA-256 metadata and streamed artifact bytes. |
| [`futures-util`](https://docs.rs/futures-util/) | Body stream adapters | Connect Hyper's incoming body to a checked backpressured response stream. |
| [`proptest`](https://proptest-rs.github.io/proptest/) (development only) | Property tests | Exercises canonical repository locator and raw-target invariants; it is not part of the release binary. |

All direct dependencies are pinned to exact versions in `Cargo.toml`; Cargo
records fully resolved transitive versions and checksums in `Cargo.lock`. The
implementation intentionally does **not** add a web router, TLS abstraction,
CLI parser, logging framework, cache, or general-purpose OCI client. Each
would add behavior and attack surface that belongs to its dedicated
implementation and security-review bead.

Runtime configuration is available through `ORAS_PROXY_*` environment
variables or a `KEY=VALUE` file selected with `ORAS_PROXY_CONFIG_FILE`.
Defaults bind to `127.0.0.1:8080`, use `https://ghcr.io`, enable no frontends,
and apply bounded request/time limits. For the local PyPI MVP only, an
explicit `ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true` permits an
`http://127.0.0.1:<port>` fixture upstream; public cleartext upstreams are
always rejected. The MVP configuration is therefore:

```sh
ORAS_PROXY_LISTEN_ADDR=127.0.0.1:0 \
ORAS_PROXY_UPSTREAM=http://127.0.0.1:<fixture-port> \
ORAS_PROXY_ALLOWED_HOSTS=127.0.0.1 \
ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true \
ORAS_PROXY_ENABLED_FRONTENDS=pypi
```

## Route contract

The public routing boundary admits only raw, origin-form `GET` and `HEAD`
targets in this shape:

```text
/r/v1/<repository-locator>/<pypi|rpm|apt|pacman>/<canonical-protocol-path>
```

`repository-locator` is the **canonical, unpadded RFC 4648 base64url**
encoding of the exact GHCR OCI repository name. For example,
`djha-skin/packages` has locator `ZGpoYS1za2luL3BhY2thZ2Vz`; an RPM metadata
request is therefore:

```text
/r/v1/ZGpoYS1za2luL3BhY2thZ2Vz/rpm/repodata/repomd.xml
```

The parser rejects noncanonical locators, percent escapes, queries, fragments,
backslashes, control characters, dot/empty/repeated path segments, path
parameters, non-v1 methods, and invalid repository spellings before an OCI
operation can be selected. It retains trailing slashes so that route-map
matching remains exact. The selected `ValidatedRepository` is an opaque typed
value; a later OCI gateway alone may use it to form a same-repository GHCR
request.

The `inbound` module supplies the next edge boundary used by the eventual
listener: bounded request-target and serialized-header size/count checks,
strict read-only framing (`GET`/`HEAD` with no request body), rejection of
conflicting framing, expectations, and upgrades, and a `Limited` body wrapper
for any future body-consuming endpoint. These checks return input-free error
codes and do not forward or retain request headers.

The `errors` module is the shared response boundary for those typed admission
failures and future fixed-origin OCI outcomes. It emits a constant generic
JSON shape with `Cache-Control: no-store`, maps malformed/unknown routes to a
non-disclosing 404, preserves `Allow: GET, HEAD` for method errors, and maps
upstream authentication, authorization, missing-content, rate-limit,
server, malformed-response, timeout, cancellation, and unexpected classes to
stable statuses. It never formats request targets, repository names, URLs,
upstream bodies, or arbitrary source errors. The only upstream response data
allowed across this boundary is `WWW-Authenticate` on a classified 401 and a
validated `Retry-After` on a 503.

Integration tests use a programmable loopback-only OCI Distribution fixture in
`crates/github-oras-packages-proxy/tests/support/registry.rs`. It serves exact
repository-scoped manifest/blob resources and can assert methods, origin-form
paths, and selected headers without retaining their values. The fixture also
supports public/private repositories, fixed `WWW-Authenticate` challenges,
conditional ETags, bounded single ranges, delayed streams, explicit status
faults, and deliberately truncated responses. Observations expose only safe
facts such as path, method, authorization presence, and expectation matching;
they never include request bodies or credential values.

## Security and operational posture

- The crate workspace denies unsafe Rust and common accidental debug output
  (`dbg!`, `println!`, `eprintln!`).
- The binary binds only the configured listener, exposes fixed health/readiness
  responses, and exits cleanly on Ctrl-C. Production telemetry and broader
  protocol coverage remain separately scoped.
- Do not place GHCR credentials in command arguments, URLs, repository files,
  test fixtures, or logs. The private GHCR credential-forwarding experiment is
  tracked separately and must use an approved secret mechanism.

## Project status

This repository is at the foundation stage. See the Beads work graph for the
implementation sequence and the versioned OCI route-map contract.

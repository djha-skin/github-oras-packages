# GitHub ORAS Packages Proxy

A small Rust service that presents one configured GitHub Packages OCI
repository as a human-readable autoindex HTTP server and an OCI Distribution
endpoint. OCI descriptor titles provide natural file paths, while an explicit
namespaced annotation opts objects into the autoindex view. Package-manager
files are ordinary visible files; the service does not need a custom route map.

The service is designed for constrained hosts such as a Raspberry Pi. It uses
bounded, backpressured streaming rather than buffering package artifacts, and
contacts only the configured OCI origin and repository. The current rewrite
slice serves standard OCI autoindex manifests at natural paths. PyPI, RPM/DNF,
Debian/APT, and Arch/pacman CRUD automation remain later milestones.

## Development prerequisites

The pinned minimum supported Rust version (MSRV) is **Rust 1.88.0**. The
repository contains `rust-toolchain.toml`, so Rustup selects that toolchain
when it is installed:

```sh
rustup toolchain install 1.88.0 --profile minimal --component cargo --component clippy --component rustfmt
```

The workspace uses the Rust 2024 edition and Cargo resolver version 3.

## Local autoindex MVP

The standard OCI-backed autoindex vertical slice is exercised by the
integration suite and smoke demo:

```sh
cargo test --test autoindex_mvp
cargo build --workspace --release
./scripts/autoindex-mvp-smoke.sh
```

The smoke command starts the checked-in Python OCI Distribution fixture and the
release `serve` binary on loopback ephemeral ports. It fetches an
`autoindex.v1` OCI manifest, browses `/` and nested directories, retrieves
ordinary files at their natural paths, checks `HEAD`, trailing-slash redirects,
unknown paths, and traversal rejection, then removes every process and
temporary path. No external network or credential is required.

The fixture's visible layers use `org.opencontainers.image.title` for their
relative path and
`io.github.djha-skin.github-oras-packages.autoindex.visible=true` for explicit
visibility. The repository is selected with `ORAS_PROXY_REPOSITORY=acme/fixture`;
there is no encoded repository segment in a public autoindex URL. The old
`pypi_mvp` test remains migration coverage for the retired route-map adapter;
native PyPI CRUD is a later milestone.

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
DEB, pacman package/database, native metadata, the legacy migration corpus, and
malformed resolver inputs. The fixture baseline is intentionally unsigned;
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
| [`serde`](https://serde.rs/) and [`serde_json`](https://serde.rs/) | OCI layout decoding | Decode bounded OCI manifests and descriptor annotations. |
| [`sha2`](https://docs.rs/sha2/) | Content verification | Verify descriptor-bound SHA-256 metadata and streamed artifact bytes. |
| [`futures-util`](https://docs.rs/futures-util/) | Body stream adapters | Connect Hyper's incoming body to a checked backpressured response stream. |
| [`proptest`](https://proptest-rs.github.io/proptest/) (development only) | Property tests | Exercises canonical repository locator and raw-target invariants; it is not part of the release binary. |

All direct dependencies are pinned to exact versions in `Cargo.toml`; Cargo
records fully resolved transitive versions and checksums in `Cargo.lock`. The
implementation intentionally does **not** add a web router, TLS abstraction,
logging framework, cache, or general-purpose OCI client. The small dependency-
free CLI currently provides `serve` and `help`; CRUD, TLS, and live registry
authentication are tracked separately in Beads.

Runtime configuration is available through `ORAS_PROXY_*` environment
variables or a `KEY=VALUE` file selected with `ORAS_PROXY_CONFIG_FILE`.
Defaults bind to `127.0.0.1:8080`, use `https://ghcr.io`, select the fixture
repository `acme/fixture` for local compatibility, and apply bounded
request/time limits. Set `ORAS_PROXY_REPOSITORY` explicitly for a real
repository. For local fixtures only, an explicit
`ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true` permits an
`http://127.0.0.1:<port>` upstream; public cleartext upstreams are always
rejected. The local configuration is therefore:

```sh
ORAS_PROXY_LISTEN_ADDR=127.0.0.1:0 \
ORAS_PROXY_REPOSITORY=acme/fixture \
ORAS_PROXY_ALLOWED_HOSTS=127.0.0.1 \
ORAS_PROXY_ALLOW_INSECURE_LOOPBACK=true \
    github-oras-packages-proxy serve
```

## Route contract

The autoindex namespace is rooted at `/` and preserves the visible OCI title:

```text
GET  /
GET  /pypi/simple/
GET  /pypi/packages/capturepkg-1.0.0-py3-none-any.whl
HEAD /pypi/packages/capturepkg-1.0.0-py3-none-any.whl
```

A descriptor is listed only when it has
`io.github.djha-skin.github-oras-packages.autoindex.visible=true`; its
`org.opencontainers.image.title` is the validated relative path. The standard
OCI namespace uses the literal configured repository separately:

```text
GET /v2/
GET /v2/acme/fixture/manifests/autoindex.v1
GET /v2/acme/fixture/blobs/sha256:<64-lowercase-hex>
```

There is no base64 repository locator, protocol-specific public prefix, or
runtime route-map lookup. See `docs/architecture/0001-*` and `0002-*` for the
full path and visibility contract. The old route parser remains only as
migration coverage while its tests are retired.

The `inbound` module supplies the next edge boundary used by the eventual
listener: bounded request-target and serialized-header size/count checks,
strict read-only framing (`GET`/`HEAD` with no request body), rejection of
conflicting framing, expectations, and upgrades, and a `Limited` body wrapper
for any future body-consuming endpoint. These checks return input-free error
codes and do not forward or retain request headers.

The `errors` module is the shared response boundary for typed admission
failures and fixed-origin OCI outcomes. It emits a constant generic JSON shape
with `Cache-Control: no-store`, maps malformed/unknown paths to a non-disclosing
404, preserves `Allow: GET, HEAD` for method errors, and maps upstream
authentication, authorization, missing-content, rate-limit, server,
malformed-response, timeout, cancellation, and unexpected classes to stable
statuses. It never formats request targets, repository names, URLs, upstream
bodies, or arbitrary source errors. The only upstream response data allowed
across this boundary is `WWW-Authenticate` on a classified 401 and a validated
`Retry-After` on a 503.

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

This repository is in the design-document rewrite. The autoindex read slice
is running; ORAS mutation, CLI CRUD, TLS/Let's Encrypt, native package-manager
CRUD, and live GitHub Packages authentication remain explicit Beads milestones.

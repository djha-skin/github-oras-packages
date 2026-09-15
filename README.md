# GitHub ORAS Packages Proxy

A small Rust reverse proxy that exposes selected package-manager repository
paths from immutable blobs published in a GitHub Container Registry (GHCR) OCI
repository. The target package protocols are PyPI, RPM/DNF, Debian/APT, and
Arch/pacman.

The proxy is designed for constrained hosts such as a Raspberry Pi. It will
use bounded, backpressured streaming rather than buffering package artifacts,
and it will contact only the configured GHCR origin. Package-specific routing,
OCI layout resolution, credentials, and serving behavior are intentionally
implemented in subsequent work items.

## Development prerequisites

The pinned minimum supported Rust version (MSRV) is **Rust 1.88.0**. The
repository contains `rust-toolchain.toml`, so Rustup selects that toolchain
when it is installed:

```sh
rustup toolchain install 1.88.0 --profile minimal --component cargo --component clippy --component rustfmt
```

The workspace uses the Rust 2024 edition and Cargo resolver version 3.

## Build and quality gates

From the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

## Dependency choices

The foundation deliberately uses a small, explicitly featured HTTP stack:

| Dependency | Purpose | Why it is suitable here |
| --- | --- | --- |
| [`tokio`](https://tokio.rs/) | Async runtime, TCP, signals | Mature non-blocking runtime; only macros, network, multi-thread runtime, and signal features are enabled. |
| [`hyper`](https://hyper.rs/) | HTTP/1 server and later HTTP bodies | Low-level HTTP primitives support streaming bodies without forcing a web framework or middleware stack. |
| [`hyper-util`](https://docs.rs/hyper-util/) | Tokio integration and server utilities | Bridges Hyper's runtime-agnostic primitives to Tokio, with only server/service/Tokio features enabled. |
| [`http-body-util`](https://docs.rs/http-body-util/) | HTTP body adapters | Provides narrow body combinators needed by the listener and response layer. |
| [`bytes`](https://docs.rs/bytes/) | Reference-counted byte buffers | Supports efficient byte chunks in the forthcoming streaming proxy path. |
| [`base64`](https://docs.rs/base64/) | Repository-locator codec | Decodes and re-encodes the canonical unpadded base64url locator without adding a URL router. |
| [`proptest`](https://proptest-rs.github.io/proptest/) (development only) | Property tests | Exercises canonical repository locator and raw-target invariants; it is not part of the release binary. |

All direct dependencies are pinned to exact versions in `Cargo.toml`; Cargo
records fully resolved transitive versions and checksums in `Cargo.lock`. The
current implementation intentionally does **not** add a web router, TLS
abstraction, JSON library, CLI parser, logging framework, cache, or OCI client.
Each adds behavior and attack surface that belongs to its dedicated
implementation and security-review bead.

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

## Security and operational posture

- The crate workspace denies unsafe Rust and common accidental debug output
  (`dbg!`, `println!`, `eprintln!`).
- The current binary starts and exits without binding a socket or emitting
  output. Listener, configuration, health endpoints, safe telemetry, and
  graceful shutdown are separate scoped work.
- Do not place GHCR credentials in command arguments, URLs, repository files,
  test fixtures, or logs. The private GHCR credential-forwarding experiment is
  tracked separately and must use an approved secret mechanism.

## Project status

This repository is at the foundation stage. See the Beads work graph for the
implementation sequence and the versioned OCI route-map contract.

# Design-document MVP migration matrix

This inventory is the implementation boundary for the rewrite described in
`design-doc.md`. Existing code is not acceptance evidence merely because it
passed the fixture-backed PyPI MVP.

| Existing surface | Current behavior | Disposition | Replacement / owner |
| --- | --- | --- | --- |
| `routing.rs` | Parses `/r/v1/<base64 locator>/<protocol>/<path>` and enables four protocol frontends | Replace | Human-readable autoindex and literal `/v2/<repository>/...` routing; `github-oras-packages-7wq`, `github-oras-packages-zxj` |
| `routing.rs::ValidatedRepository::locator` | Canonical base64url repository encoding | Remove from runtime | Configured literal repository; `github-oras-packages-6nr` |
| `oci.rs::Snapshot` | Fetches fixed `oras-packages.v1`, config, and custom route-map layer | Replace | Standard OCI manifest/descriptors with title plus visibility annotation; `github-oras-packages-csv`, `github-oras-packages-zxj` |
| `oci.rs::parse_route_map` | Runtime protocol/path-to-digest lookup | Remove from runtime | Manifest descriptor projection; `github-oras-packages-u03` |
| `proxy.rs` | PyPI-only route-map lookup and artifact response | Replace | Generic autoindex directory/file response plus OCI API; `github-oras-packages-7wq`, `github-oras-packages-zxj` |
| `config.rs` | Environment/file config for fixed origin and enabled package protocols | Refactor | `serve` CLI/config with explicit repository, hostname, TLS, credentials, and storage policy; `github-oras-packages-us7` |
| `server.rs` | Hyper HTTP/1 listener with health/readiness and read-only dispatch | Retain/refactor | Add autoindex, OCI mutation, TLS, ACME, logging; `github-oras-packages-zxj`, `github-oras-packages-1av`, `github-oras-packages-jik` |
| `main.rs` | Environment-only binary and Ctrl-C lifecycle | Replace | CLI with `serve` and `autoindex create/update/delete`; `github-oras-packages-gqv` |
| `errors.rs` | Safe fixed responses for route-map proxy errors | Retain/refactor | Generic HTTP/OCI/autoindex error taxonomy and diagnostics; `github-oras-packages-jik` |
| `inbound.rs` | Bounded read-only request admission and body limits | Retain/refactor | Reuse for GET/HEAD and bounded OCI upload requests; `github-oras-packages-zxj` |
| Fixture registry | Exact read resources, route-map manifest, limited auth observations | Replace/extend | Standard OCI Distribution upload/manifest/delete fixture; `github-oras-packages-4v9`, `github-oras-packages-zxj` |
| `fixtures/corpus/route-map.json` | Checked-in custom runtime route index | Retire | Native file tree publication fixture; `github-oras-packages-csv` |
| `scripts/pypi_fixture.py` | Loopback OCI fixture with standard autoindex mode | Refactor | Generic OCI fixture usable by ORAS and autoindex tests; `github-oras-packages-4v9` |
| `scripts/autoindex-mvp-smoke.sh` | Natural-path autoindex smoke test | Retain/refactor | ORAS + autoindex + CRUD + native pip demo; `github-oras-packages-4v9`, `github-oras-packages-l61` |
| `tests/pypi_mvp.rs` | PyPI route-map integration coverage | Migrate/defer | Native autoindex and package-client acceptance; `github-oras-packages-l61` |
| `tests/fixture_corpus.rs` | Route-map and all-protocol fixture invariants | Retain selectively | Standard OCI descriptor/path tests; old route-map cases become migration regressions |
| README and fixture docs | Describe base64 locator and custom route-map MVP | Rewrite | `github-oras-packages-s5f` |
| GitHub auth | Optional forwarded `Authorization` header | Replace | Native bearer challenge/token exchange; `github-oras-packages-1kk` |
| Storage | No upload path; read-only upstream proxy | Replace | Stateless registry-backed uploads with bounded temporary state; `github-oras-packages-smq` |
| TLS | Not implemented | Add | `github-oras-packages-1av` |
| Observability | Minimal fixed health/errors, no request telemetry | Add | `github-oras-packages-jik` |

## Explicit non-goals for this MVP

The design document makes package-manager-specific CRUD the later milestones:

1. PyPI CRUD is Milestone 1 after the autoindex substrate.
2. DNF/RPM CRUD is Milestone 2.
3. APT, pacman, and other package-manager CRUD follow separately.

The MVP must prove that ordinary package-manager-produced files work through
an autoindex server. It does not need protocol-specific create/update/delete
logic before the autoindex CLI and OCI publication substrate are complete.

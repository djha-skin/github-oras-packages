# ADR 0001: Human-readable autoindex routing and configured repository scope

- Status: accepted for the design-document MVP
- Date: 2026-09-20
- Scope: public HTTP routes, OCI Distribution routes, repository selection, and
  the transition away from the fixture-era route map

## Decision

A `serve` process serves exactly one configured OCI repository. Repository
selection is configuration, not data encoded into a public request path. The
configured repository is the literal OCI repository name, such as
`acme/fixture`, and is validated before the listener starts.

The service exposes two related but separate URL namespaces:

```text
GET /<relative-file-or-directory-path>
GET /v2/
GET /v2/<configured-repository>/manifests/<reference>
GET /v2/<configured-repository>/blobs/<sha256-digest>
```

The autoindex namespace is rooted at `/`. A stored object named
`pypi/simple/capturepkg/index.html` is browsed at exactly
`/pypi/simple/capturepkg/index.html`; no protocol prefix, repository locator,
base64/base58 segment, or route-map lookup is inserted. Directory requests use
a trailing slash and return deterministic HTML links containing the stored
relative child paths. A request without the trailing slash redirects to the
slash form when the target is a known directory.

The OCI namespace follows the OCI Distribution API. Its repository component is
the configured literal repository name, so a repository named `acme/fixture`
uses `/v2/acme/fixture/...`. This is an OCI protocol route, not an autoindex
path and not a second repository-selection mechanism. A request for a
repository other than the configured one is rejected without contacting the
upstream registry.

The initial public-host deployment should use one hostname per configured
repository. A future multi-repository gateway may add an explicit configured
host-to-repository map, but it must not infer repository identity from an
encoded path segment.

## Publication and visibility

The authoritative autoindex path is the validated relative path represented by
an OCI descriptor's materialization/path metadata. Descriptor annotations may
mark an object as visible to the autoindex view and may carry optional display
metadata, but annotations do not replace the path and do not form a route map.
The visibility annotation contract is defined separately by ADR 0002.

The runtime does not require `route-map.json`, a custom route-map media type,
or a protocol/path-to-digest index. Digests identify blob bytes; OCI manifests
and descriptors identify the blobs and their relative names. Package managers
remain responsible for producing their ordinary index and metadata files. The
proxy serves those files as opaque bytes.

For an OCI artifact pushed with ORAS, the descriptor title/path and the
project's visibility annotation are sufficient for the autoindex projection.
An artifact without the visibility marker remains available through ordinary
OCI manifest/blob operations but is not listed in the autoindex namespace.

## Path rules

* Paths are UTF-8 request paths after the HTTP parser's normal request-target
  admission, but percent-encoded separators, dot segments, backslashes,
  controls, query strings in an origin-form path, and ambiguous empty segments
  are rejected rather than normalized.
* Stored paths are relative, use `/` separators, do not begin with `/`, and do
  not contain `.` or `..` segments. A single final `/` is a directory spelling,
  not part of a file name.
* Directory listings escape link text and href attributes. Listing order is
  deterministic and directory entries are not allowed to shadow files.
* OCI digest and reference validation remains independent from autoindex path
  validation. A valid OCI blob digest does not make an unsafe HTTP path valid.

## Compatibility and migration

The old `/r/v1/<base64url-repository>/<protocol>/<path>` namespace is retired
by this design. It must not be added to new code or documented as a supported
route. Existing fixture-era tests that only prove route-map behavior are
migration tests until replaced by native OCI/autoindex tests; they are not
acceptance evidence for this MVP.

The low-level work for bounded HTTP bodies, fixed-origin requests, digest
verification, safe upstream error classification, and authorization redaction
may be retained and refactored. The custom route-map snapshot resolver,
protocol-specific route admission, and base64 locator codec must be removed or
isolated behind legacy tests during the rewrite and must not be used by the
new runtime.

## Examples

For a server configured with upstream repository `acme/fixture`:

```text
Autoindex root:       http://localhost:8080/
Package file:          http://localhost:8080/pypi/simple/capturepkg/index.html
OCI API check:         http://localhost:8080/v2/
OCI manifest:          http://localhost:8080/v2/acme/fixture/manifests/demo
OCI blob:              http://localhost:8080/v2/acme/fixture/blobs/sha256:<64-lowercase-hex>
```

The package file's route ends with `pypi/simple/capturepkg/index.html`, exactly
as it appears in the publication. The repository name is visible in the OCI
route because it is part of the standard OCI API; it is never base64 encoded.

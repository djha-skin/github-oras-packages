# ADR 0002: Autoindex visibility and descriptor path projection

- Status: proposed implementation contract for the design-document MVP
- Date: 2026-09-20
- Scope: which OCI descriptors appear in autoindex listings and how names map
  to HTTP paths

## Contract

An autoindex-visible OCI descriptor carries the namespaced annotation:

```text
io.github.djha-skin.github-oras-packages.autoindex.visible=true
```

The value is exactly `true` (lowercase ASCII). Any other value, including a
missing annotation, means the descriptor is not listed or served through the
autoindex namespace. Standard `org.opencontainers.*` annotations remain
available for their standard meanings; none of them alone grants autoindex
visibility.

The visible path is the descriptor's `org.opencontainers.image.title` value.
It is a relative slash-separated path validated by the same rules as an
incoming autoindex path. The title is therefore a materialization/path hint
with application-level validation, not a semantic package-manager route map.
A publisher may use a different OCI annotation for source metadata, version,
or display text, but the visible path remains the title.

A manifest may contain any number of visible and invisible descriptors. The
projection includes only visible descriptors. Config descriptors are not
listed as files unless explicitly represented as an ordinary visible layer;
OCI manifests, configs, signatures, and other non-visible objects remain
accessible through OCI API operations according to their own references.

## Validation rules

The service rejects a publication for autoindex projection if a visible title:

* is empty, absolute, or contains a query, fragment, percent escape, backslash,
  control character, or semicolon;
* contains `.` or `..` segments, repeated interior slashes, or an unsafe UTF-8
  spelling;
* exceeds the configured path/segment limits;
* collides with another visible title; or
* makes one path both a file and an ancestor directory of another file.

A descriptor without `org.opencontainers.image.title` is ignored by the
filesystem projection even if it has the visibility annotation. This keeps
`oras pull` behavior and autoindex behavior understandable: visibility is an
explicit opt-in and the title is the path.

Directory entries are synthesized from visible file titles at request time.
Empty directories have no representation because OCI descriptors represent
objects, not directory metadata. A directory listing is deterministic,
HTML-escaped, and links to either child files or child directories.

## Rationale

The visibility bit prevents ordinary container images, signatures, configs, or
unrelated OCI artifacts in the same GitHub Packages account from appearing as
files. The title preserves the human-readable autoindex invariant and gives
ORAS a useful materialization name. A route map is unnecessary: the manifest
already lists descriptors, the title supplies the relative name, and the blob
digest supplies immutable bytes.

The annotation is intentionally namespaced and narrowly scoped. It does not
encode package-manager semantics, authentication, repository selection, or a
second path. PyPI, apt, dnf, pacman, and custom package managers publish their
normal index files and archives as ordinary visible paths.

## Examples

A visible layer descriptor:

```json
{
  "mediaType": "text/html",
  "digest": "sha256:<64-lowercase-hex>",
  "size": 78,
  "annotations": {
    "org.opencontainers.image.title": "pypi/simple/index.html",
    "io.github.djha-skin.github-oras-packages.autoindex.visible": "true"
  }
}
```

A layer with `org.opencontainers.image.title` but no visibility annotation is
still a normal OCI layer but is absent from `/pypi/` and autoindex listings.

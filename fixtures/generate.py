#!/usr/bin/env python3
"""Build the deterministic, redistributable package/OCI fixture corpus.

The corpus is intentionally synthetic and contains no third-party code or
credentials.  It uses only the Python standard library.  Run from the
repository root with ``python3 fixtures/generate.py``.  Existing generated
files under ``fixtures/corpus`` are replaced.
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import io
import json
import shutil
import struct
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CORPUS = ROOT / "corpus"
BLOB_DIR = CORPUS / "blobs" / "sha256"
PACKAGE = "capturepkg"
VERSION = "1.0.0"
RELEASE = "1"
REPOSITORY = "acme/fixture"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=True, separators=(",", ":")) + "\n").encode()


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def add_blob(data: bytes, blobs: dict[str, dict[str, object]]) -> str:
    digest = sha256(data)
    path = BLOB_DIR / digest
    if path.exists() and path.read_bytes() != data:
        raise RuntimeError(f"digest collision for {digest}")
    write(path, data)
    blobs[digest] = {"digest": f"sha256:{digest}", "size": len(data)}
    return f"sha256:{digest}"


def gzip_bytes(data: bytes) -> bytes:
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0, compresslevel=9) as stream:
        stream.write(data)
    return output.getvalue()


def tar_bytes(files: list[tuple[str, bytes, int]]) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data, mode in files:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.uname = "root"
            info.gname = "root"
            info.mtime = 0
            archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def zip_bytes(files: list[tuple[str, bytes]]) -> bytes:
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, data in files:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
    return output.getvalue()


def cpio_newc(files: list[tuple[str, bytes, int]]) -> bytes:
    """Create a deterministic cpio ``newc`` archive for an RPM payload."""
    output = bytearray()
    inode = 1
    for name, data, mode in files:
        name_bytes = name.encode() + b"\0"
        fields = [
            0x070701,
            inode,
            mode,
            0,
            0,
            1,
            0,
            len(data),
            0,
            0,
            0,
            0,
            len(name_bytes),
            0,
        ]
        output.extend("".join(f"{field:08x}" for field in fields).encode())
        output.extend(name_bytes)
        output.extend(b"\0" * ((-len(output)) % 4))
        output.extend(data)
        output.extend(b"\0" * ((-len(output)) % 4))
        inode += 1
    trailer = b"TRAILER!!!\0"
    fields = [0x070701, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, len(trailer), 0]
    output.extend("".join(f"{field:08x}" for field in fields).encode())
    output.extend(trailer)
    output.extend(b"\0" * ((-len(output)) % 4))
    return bytes(output)


def zstd_raw(data: bytes) -> bytes:
    """Encode a zstd frame using deterministic raw (uncompressed) blocks.

    Raw blocks are valid zstd and avoid depending on a platform compressor.
    The frame content size is encoded in the four-byte form, and each block
    is below zstd's 128 KiB maximum.
    """
    output = bytearray(b"\x28\xb5\x2f\xfd")
    output.append(0xA0)  # four-byte content size + single-segment frame
    output.extend(struct.pack("<I", len(data)))
    if not data:
        output.extend(b"\x01\x00\x00")
        return bytes(output)
    for offset in range(0, len(data), 128 * 1024):
        chunk = data[offset : offset + 128 * 1024]
        last = offset + len(chunk) == len(data)
        header = (len(chunk) << 3) | (1 if last else 0)  # raw block type = 0
        output.extend(header.to_bytes(3, "little"))
        output.extend(chunk)
    return bytes(output)


def ar_member(name: str, data: bytes, mode: int = 0o100644) -> bytes:
    if len(name) > 16 or any(char in name for char in " /\n"):
        raise ValueError(f"unsupported ar member name: {name}")
    header = (
        f"{name + '/':<16}"
        f"{0:<12}"
        f"{0:<6}"
        f"{0:<6}"
        f"{format(mode, 'o'):<8}"
        f"{len(data):<10}`\n"
    ).encode("ascii")
    if len(header) != 60:
        raise AssertionError(f"bad ar header length: {len(header)}")
    return header + data + (b"\n" if len(data) % 2 else b"")


def debian_package() -> bytes:
    control = (
        "Package: capturepkg\n"
        "Version: 1.0.0-1\n"
        "Section: misc\n"
        "Priority: optional\n"
        "Architecture: amd64\n"
        "Maintainer: Fixture Maintainer <fixture@example.invalid>\n"
        "Description: A tiny deterministic package fixture\n"
        " This package contains only fixture data.\n"
    ).encode()
    control_tar = gzip_bytes(tar_bytes([("control", control, 0o100644)]))
    data_tar = gzip_bytes(
        tar_bytes(
            [
                ("./usr/share/doc/capturepkg/README", b"capturepkg fixture\n", 0o100644),
            ]
        )
    )
    return b"!<arch>\n" + b"".join(
        [
            ar_member("debian-binary", b"2.0\n"),
            ar_member("control.tar.gz", control_tar),
            ar_member("data.tar.gz", data_tar),
        ]
    )


def rpm_header(entries: list[tuple[int, int, object]]) -> bytes:
    """Serialize an RPM header (the format is specified by rpm.org)."""
    # RPM header data types and their required alignment.
    alignment = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 1, 9: 1}
    chunks: list[bytes] = []
    indexes: list[tuple[int, int, int, int]] = []
    data_size = 0
    for tag, kind, value in entries:
        padding = (-data_size) % alignment[kind]
        chunks.append(b"\0" * padding)
        data_size += padding
        offset = data_size
        if kind == 2:  # INT8
            encoded = bytes(value)
            count = len(encoded)
        elif kind == 3:  # INT16
            values = list(value)
            encoded = b"".join(struct.pack(">H", item) for item in values)
            count = len(values)
        elif kind == 4:  # INT32
            values = list(value)
            encoded = b"".join(struct.pack(">I", item) for item in values)
            count = len(values)
        elif kind == 5:  # INT64
            values = list(value)
            encoded = b"".join(struct.pack(">Q", item) for item in values)
            count = len(values)
        elif kind == 6:  # STRING
            encoded = str(value).encode() + b"\0"
            count = 1
        elif kind == 7:  # BIN
            encoded = bytes(value)
            count = len(encoded)
        elif kind in (8, 9):  # STRING_ARRAY / I18NSTRING
            values = list(value)
            encoded = b"".join(str(item).encode() + b"\0" for item in values)
            count = len(values)
        else:
            raise ValueError(f"unsupported RPM header type {kind}")
        chunks.append(encoded)
        data_size += len(encoded)
        indexes.append((tag, kind, offset, count))
    index_data = b"".join(struct.pack(">IIII", *item) for item in indexes)
    data = b"".join(chunks)
    return struct.pack(">8sII", b"\x8e\xad\xe8\x01\0\0\0\0", len(indexes), len(data)) + index_data + data


def rpm_lead() -> bytes:
    # The lead is legacy and mostly informational; x86_64's arch number is 1.
    return struct.pack(">4sBBHH66sHH16s", b"\xed\xab\xee\xdb", 3, 0, 0, 1, b"capturepkg-1.0.0-1", 1, 5, b"\0" * 16)


def rpm_package() -> bytes:
    payload = gzip_bytes(cpio_newc([("./usr/share/doc/capturepkg/README", b"capturepkg fixture\n", 0o100644)]))
    payload_digest = hashlib.sha256(payload).hexdigest()
    files = [("./usr/share/doc/capturepkg/README", b"capturepkg fixture\n", 0o100644)]
    header = rpm_header(
        [
            (1000, 6, PACKAGE),
            (1001, 6, VERSION),
            (1002, 6, RELEASE),
            (1004, 9, ["A tiny deterministic package fixture"]),
            (1005, 9, ["This package contains only fixture data."]),
            (1006, 4, [0]),
            (1007, 6, "fixture-builder"),
            (1009, 4, [len(b"capturepkg fixture\n")]),
            (1014, 6, "MIT"),
            (1016, 9, ["Unspecified"]),
            (1021, 6, "linux"),
            (1022, 6, "x86_64"),
            (1028, 4, [len(data) for _, data, _ in files]),
            (1030, 3, [mode for _, _, mode in files]),
            (1034, 4, [0 for _ in files]),
            (1035, 8, [hashlib.sha256(data).hexdigest() for _, data, _ in files]),
            (1037, 4, [0 for _ in files]),
            (1039, 8, ["root" for _ in files]),
            (1040, 8, ["root" for _ in files]),
            (1045, 4, [0 for _ in files]),
            (1046, 4, [len(payload)]),
            (5092, 8, [payload_digest]),
            (5112, 4, [len(payload)]),
            (1116, 4, [0 for _ in files]),
            (1117, 8, ["README" for _ in files]),
            (1118, 8, ["./usr/share/doc/capturepkg/" for _ in files]),
            (1124, 6, "cpio"),
            (1125, 6, "gzip"),
            (1126, 6, "9"),
            (5011, 4, [8]),
        ]
    )
    signature = rpm_header(
        [
            (257, 4, [len(header) + len(payload)]),
            (261, 7, hashlib.md5(header + payload).digest()),
            (269, 6, hashlib.sha1(header).hexdigest()),
            (273, 6, hashlib.sha256(header).hexdigest()),
        ]
    )
    signature += b"\0" * ((-len(signature)) % 8)
    return rpm_lead() + signature + header + payload


def wheel_package() -> bytes:
    metadata = (
        "Metadata-Version: 2.1\n"
        "Name: capturepkg\n"
        "Version: 1.0.0\n"
        "Summary: A tiny deterministic package fixture\n"
        "License: MIT\n"
    ).encode()
    wheel = b"Wheel-Version: 1.0\nGenerator: fixture-builder\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
    module = b"VALUE = 'fixture'\n"
    files = [
        ("capturepkg/__init__.py", module),
        ("capturepkg-1.0.0.dist-info/METADATA", metadata),
        ("capturepkg-1.0.0.dist-info/WHEEL", wheel),
    ]
    records = []
    for name, data in files:
        encoded_digest = hashlib.sha256(data).digest()
        digest = base64.urlsafe_b64encode(encoded_digest).rstrip(b"=").decode()
        records.append(f"{name},sha256={digest},{len(data)}")
    records.append("capturepkg-1.0.0.dist-info/RECORD,,")
    record = ("\n".join(records) + "\n").encode()
    return zip_bytes(files + [("capturepkg-1.0.0.dist-info/RECORD", record)])


def sdist_package() -> bytes:
    prefix = "capturepkg-1.0.0/"
    pkg_info = (
        "Metadata-Version: 2.1\nName: capturepkg\nVersion: 1.0.0\n"
        "Summary: A tiny deterministic package fixture\n"
    ).encode()
    setup_cfg = b"[metadata]\nname = capturepkg\nversion = 1.0.0\n"
    pyproject = b"[build-system]\nrequires = []\nbuild-backend = 'setuptools.build_meta'\n"
    return gzip_bytes(
        tar_bytes(
            [
                (prefix + "PKG-INFO", pkg_info, 0o100644),
                (prefix + "setup.cfg", setup_cfg, 0o100644),
                (prefix + "pyproject.toml", pyproject, 0o100644),
            ]
        )
    )


def pacman_package() -> bytes:
    pkginfo = (
        "pkgname = capturepkg\n"
        "pkgver = 1.0.0-1\n"
        "pkgdesc = A tiny deterministic package fixture\n"
        "builddate = 0\n"
        "packager = Fixture Maintainer <fixture@example.invalid>\n"
        "size = 19\n"
        "arch = x86_64\n"
        "license = MIT\n"
    ).encode()
    return zstd_raw(
        tar_bytes(
            [
                (".PKGINFO", pkginfo, 0o100644),
                ("usr/share/doc/capturepkg/README", b"capturepkg fixture\n", 0o100644),
            ]
        )
    )


def pacman_database() -> bytes:
    desc = (
        "%NAME%\ncapturepkg\n\n%VERSION%\n1.0.0-1\n\n"
        "%DESC%\nA tiny deterministic package fixture\n\n"
        "%CSIZE%\n0\n\n%ISIZE%\n19\n\n%ARCH%\nx86_64\n"
    ).encode()
    files = b"%FILES%\nusr/share/doc/capturepkg/README\n"
    return zstd_raw(
        tar_bytes(
            [
                ("capturepkg-1.0.0-1/desc", desc, 0o100644),
                ("capturepkg-1.0.0-1/files", files, 0o100644),
            ]
        )
    )


def build() -> None:
    if CORPUS.exists():
        shutil.rmtree(CORPUS)
    BLOB_DIR.mkdir(parents=True)
    blobs: dict[str, dict[str, object]] = {}
    routes: list[dict[str, object]] = []

    def route(protocol: str, path: str, media_type: str, data: bytes) -> None:
        digest = add_blob(data, blobs)
        routes.append(
            {
                "protocol": protocol,
                "path": path,
                "descriptor": {"mediaType": media_type, "digest": digest, "size": len(data)},
            }
        )

    wheel = wheel_package()
    sdist = sdist_package()
    rpm = rpm_package()
    deb = debian_package()
    pacman = pacman_package()
    pacman_db = pacman_database()

    wheel_digest = sha256(wheel)
    sdist_digest = sha256(sdist)
    rpm_digest = sha256(rpm)
    deb_digest = sha256(deb)
    pacman_digest = sha256(pacman)
    pacman_db_digest = sha256(pacman_db)
    write(CORPUS / "packages" / "capturepkg-1.0.0-py3-none-any.whl", wheel)
    write(CORPUS / "packages" / "capturepkg-1.0.0.tar.gz", sdist)
    write(CORPUS / "packages" / "capturepkg-1.0.0-1.x86_64.rpm", rpm)
    write(CORPUS / "packages" / "capturepkg_1.0.0-1_amd64.deb", deb)
    write(CORPUS / "packages" / "capturepkg-1.0.0-1-x86_64.pkg.tar.zst", pacman)

    route("pypi", "simple/", "text/html; charset=utf-8", b"<!doctype html><html><body><a href=\"capturepkg/\">capturepkg</a></body></html>\n")
    route(
        "pypi",
        "simple/capturepkg/",
        "text/html; charset=utf-8",
        f'<!doctype html><html><body><a href="/r/v1/YWNtZS9maXh0dXJl/pypi/packages/capturepkg-1.0.0-py3-none-any.whl#sha256={wheel_digest}">capturepkg-1.0.0-py3-none-any.whl</a><br><a href="/r/v1/YWNtZS9maXh0dXJl/pypi/packages/capturepkg-1.0.0.tar.gz#sha256={sdist_digest}">capturepkg-1.0.0.tar.gz</a></body></html>\n'.encode(),
    )
    route("pypi", "packages/capturepkg-1.0.0-py3-none-any.whl", "application/octet-stream", wheel)
    route("pypi", "packages/capturepkg-1.0.0.tar.gz", "application/gzip", sdist)

    primary_xml = f"""<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" packages=\"1\"><package type=\"rpm\"><name>capturepkg</name><arch>x86_64</arch><version epoch=\"0\" ver=\"1.0.0\" rel=\"1\"/><location href=\"packages/capturepkg-1.0.0-1.x86_64.rpm\"/><checksum type=\"sha256\" pkgid=\"YES\">{rpm_digest}</checksum><size package=\"{len(rpm)}\"/></package></metadata>\n""".encode()
    primary_gz = gzip_bytes(primary_xml)
    repomd = f"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\"><data type=\"primary\"><checksum type=\"sha256\">{sha256(primary_gz)}</checksum><location href=\"repodata/primary.xml.gz\"/><size>{len(primary_gz)}</size></data></repomd>\n".encode()
    route("rpm", "repodata/primary.xml.gz", "application/gzip", primary_gz)
    route("rpm", "repodata/repomd.xml", "application/xml", repomd)
    route("rpm", "packages/capturepkg-1.0.0-1.x86_64.rpm", "application/x-rpm", rpm)

    packages_gz = gzip_bytes(
        f"Package: capturepkg\nVersion: 1.0.0-1\nArchitecture: amd64\nFilename: pool/main/c/capturepkg/capturepkg_1.0.0-1_amd64.deb\nSize: {len(deb)}\nSHA256: {deb_digest}\nDescription: A tiny deterministic package fixture\n\n".encode()
    )
    route("apt", "dists/stable/Release", "text/plain; charset=utf-8", f"Origin: fixture\nSuite: stable\nSHA256:\n {sha256(packages_gz)} {len(packages_gz)} main/binary-amd64/Packages.gz\n".encode())
    route("apt", "dists/stable/main/binary-amd64/Packages.gz", "application/gzip", packages_gz)
    route("apt", "pool/main/c/capturepkg/capturepkg_1.0.0-1_amd64.deb", "application/vnd.debian.binary-package", deb)

    route("pacman", "capture.db", "application/octet-stream", pacman_db)
    route("pacman", "capturepkg-1.0.0-1-x86_64.pkg.tar.zst", "application/zstd", pacman)

    config = b'{"kind":"github-oras-packages-layout","version":1}'
    config_digest = add_blob(config, blobs)
    route_map = {"version": 1, "routes": routes}
    route_map_bytes = canonical_json(route_map)
    route_map_digest = add_blob(route_map_bytes, blobs)
    manifest = {
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.github.oras-packages.layout.v1+json",
            "digest": config_digest,
            "size": len(config),
        },
        "layers": [
            {
                "mediaType": "application/vnd.github.oras-packages.route-map.v1+json",
                "digest": route_map_digest,
                "size": len(route_map_bytes),
            }
        ],
    }
    manifest_bytes = canonical_json(manifest)
    manifest_digest = add_blob(manifest_bytes, blobs)
    index = {
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": len(manifest_bytes),
                "annotations": {"org.opencontainers.image.ref.name": "oras-packages.v1"},
            }
        ],
    }
    write(CORPUS / "oci-layout", b'{"imageLayoutVersion":"1.0.0"}\n')
    write(CORPUS / "index.json", canonical_json(index))
    write(CORPUS / "manifest.json", manifest_bytes)
    write(CORPUS / "route-map.json", route_map_bytes)
    write(CORPUS / "config.json", config)
    write(CORPUS / "metadata" / "primary.xml", primary_xml)
    write(CORPUS / "metadata" / "repomd.xml", repomd)
    write(CORPUS / "metadata" / "Packages.gz", packages_gz)
    write(CORPUS / "metadata" / "Release", f"Origin: fixture\nSuite: stable\nSHA256:\n {sha256(packages_gz)} {len(packages_gz)} main/binary-amd64/Packages.gz\n".encode())
    write(CORPUS / "metadata" / "capture.db", pacman_db)

    # These are complete, independent negative inputs consumed by resolver tests.
    malformed = CORPUS / "malformed"
    write(malformed / "route-map-duplicate.json", canonical_json({"version": 1, "routes": [routes[0], routes[0]]}))
    foreign = json.loads(json.dumps(routes[0]))
    foreign["descriptor"]["repository"] = "evil/foreign"
    write(malformed / "route-map-foreign-descriptor.json", canonical_json({"version": 1, "routes": [foreign]}))
    bad_version = {"version": 2, "routes": []}
    write(malformed / "route-map-unsupported-version.json", canonical_json(bad_version))
    extra_layer = json.loads(json.dumps(manifest))
    extra_layer["layers"].append(extra_layer["layers"][0])
    write(malformed / "manifest-extra-layer.json", canonical_json(extra_layer))
    missing = json.loads(json.dumps(manifest))
    missing["config"]["digest"] = "sha256:" + ("0" * 64)
    missing["config"]["size"] = len(config)
    write(malformed / "manifest-missing-config.json", canonical_json(missing))
    mismatch_digest = "sha256:" + sha256(b"declared bytes")
    write(malformed / "blob-mismatch.json", canonical_json({"declared": mismatch_digest, "actual": "sha256:" + sha256(b"actual bytes")}))
    write(malformed / "invalid-json.json", b'{"version":1,"routes":[}')
    write(malformed / "bad-descriptor.json", canonical_json({"version": 1, "routes": [{"protocol": "pypi", "path": "simple/", "descriptor": {"mediaType": "text/plain", "digest": "md5:not-sha256", "size": -1}}]}))
    write(malformed / "metadata-external-path.json", canonical_json({"protocol": "apt", "path": "dists/stable/Release", "filename": "https://evil.example.invalid/Packages.gz"}))
    write(malformed / "metadata-checksum-mismatch.json", canonical_json({"protocol": "rpm", "path": "repodata/primary.xml.gz", "declaredSha256": "0" * 64, "actualSha256": sha256(primary_gz)}))

    package_index = {
        "repository": REPOSITORY,
        "locator": "YWNtZS9maXh0dXJl",
        "manifest": {"digest": manifest_digest, "size": len(manifest_bytes), "reference": "oras-packages.v1"},
        "config": {"digest": config_digest, "size": len(config)},
        "routeMap": {"digest": route_map_digest, "size": len(route_map_bytes), "routeCount": len(routes)},
        "packages": [
            {"path": "packages/capturepkg-1.0.0-py3-none-any.whl", "digest": f"sha256:{wheel_digest}", "format": "wheel"},
            {"path": "packages/capturepkg-1.0.0.tar.gz", "digest": f"sha256:{sdist_digest}", "format": "sdist"},
            {"path": "packages/capturepkg-1.0.0-1.x86_64.rpm", "digest": f"sha256:{rpm_digest}", "format": "rpm"},
            {"path": "packages/capturepkg_1.0.0-1_amd64.deb", "digest": f"sha256:{deb_digest}", "format": "deb"},
            {"path": "packages/capturepkg-1.0.0-1-x86_64.pkg.tar.zst", "digest": f"sha256:{pacman_digest}", "format": "pacman"},
            {"path": "metadata/capture.db", "digest": f"sha256:{pacman_db_digest}", "format": "pacman-db"},
        ],
        "signaturePolicy": "unsigned-base-fixture",
    }
    write(CORPUS / "fixture-index.json", canonical_json(package_index))

    # Validate every content-addressed file before returning successfully.
    for path in sorted(BLOB_DIR.iterdir()):
        if path.name != sha256(path.read_bytes()):
            raise AssertionError(f"blob hash mismatch: {path}")
    print(f"wrote {len(list(BLOB_DIR.iterdir()))} blobs and {len(routes)} route entries to {CORPUS}")


if __name__ == "__main__":
    build()

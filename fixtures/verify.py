#!/usr/bin/env python3
"""Verify generated fixture bytes and native package/container structure."""

from __future__ import annotations

import base64
import gzip
import hashlib
import io
import json
import struct
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent / "corpus"
PACKAGE_DIR = ROOT / "packages"
BLOB_DIR = ROOT / "blobs" / "sha256"


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def assert_true(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def ar_members(data: bytes) -> dict[str, bytes]:
    assert_true(data.startswith(b"!<arch>\n"), "DEB must use ar archive magic")
    members: dict[str, bytes] = {}
    offset = 8
    while offset < len(data):
        header = data[offset : offset + 60]
        assert_true(len(header) == 60 and header[58:60] == b"`\n", "invalid ar member")
        name = header[:16].decode("ascii").rstrip(" /")
        size = int(header[48:58].decode("ascii").strip())
        start = offset + 60
        members[name] = data[start : start + size]
        assert_true(len(members[name]) == size, "ar member exceeds DEB")
        offset = start + size + size % 2
    assert_true(offset == len(data), "ar member exceeds DEB")
    return members


def zstd_raw_payload(data: bytes) -> bytes:
    assert_true(data[:4] == b"\x28\xb5\x2f\xfd", "not a zstd frame")
    descriptor = data[4]
    assert_true(descriptor == 0xA0, "fixture zstd descriptor changed")
    size = struct.unpack("<I", data[5:9])[0]
    offset = 9
    output = bytearray()
    while offset < len(data):
        header = int.from_bytes(data[offset : offset + 3], "little")
        offset += 3
        last = bool(header & 1)
        block_size = header >> 3
        assert_true((header & 0x7) in (0, 1), "fixture zstd block is not raw")
        output.extend(data[offset : offset + block_size])
        assert_true(len(data[offset : offset + block_size]) == block_size, "truncated zstd block")
        offset += block_size
        if last:
            assert_true(offset == len(data), "zstd trailing bytes")
            break
    assert_true(len(output) == size, "zstd content size mismatch")
    return bytes(output)


def main() -> None:
    fixture_index = json.loads((ROOT / "fixture-index.json").read_text())
    manifest = json.loads((ROOT / "manifest.json").read_text())
    route_map = json.loads((ROOT / "route-map.json").read_text())
    assert_true(fixture_index["repository"] == "acme/fixture", "repository changed")
    assert_true(fixture_index["locator"] == "YWNtZS9maXh0dXJl", "locator changed")
    assert_true(manifest["schemaVersion"] == 2, "not an OCI v1 manifest")
    assert_true(manifest["mediaType"] == "application/vnd.oci.image.manifest.v1+json", "bad manifest type")
    assert_true(len(manifest["layers"]) == 1, "OCI fixture must have one route-map layer")
    assert_true(route_map["version"] == 1 and len(route_map["routes"]) == 12, "route map changed")
    assert_true(all(entry["path"] and not entry["path"].startswith("/") for entry in route_map["routes"]), "route map path must be relative")

    for path in BLOB_DIR.iterdir():
        assert_true(path.is_file() and path.name == digest(path.read_bytes()), f"bad blob {path}")

    blob_by_digest = {f"sha256:{p.name}": p.read_bytes() for p in BLOB_DIR.iterdir()}
    for entry in route_map["routes"]:
        descriptor = entry["descriptor"]
        data = blob_by_digest[descriptor["digest"]]
        assert_true(len(data) == descriptor["size"], f"size mismatch for {entry['path']}")
        assert_true(entry["protocol"] in {"pypi", "rpm", "apt", "pacman"}, "unknown protocol")
        assert_true(set(descriptor) == {"mediaType", "digest", "size"}, f"descriptor fields changed for {entry['path']}")
        assert_true("/" not in descriptor.get("repository", ""), "foreign descriptor")
    for descriptor in [manifest["config"], manifest["layers"][0]]:
        blob = blob_by_digest[descriptor["digest"]]
        assert_true(len(blob) == descriptor["size"], "manifest descriptor mismatch")
        assert_true(digest(blob) == descriptor["digest"].split(":", 1)[1], "manifest blob digest mismatch")
    manifest_digest = fixture_index["manifest"]["digest"].split(":", 1)[1]
    assert_true(digest((ROOT / "manifest.json").read_bytes()) == manifest_digest, "manifest copy changed")

    package_paths = {entry["path"]: entry["digest"] for entry in fixture_index["packages"]}
    for path, expected in package_paths.items():
        package = (ROOT / path).read_bytes() if path.startswith("metadata/") else (ROOT / path).read_bytes()
        assert_true(digest(package) == expected.split(":", 1)[1], f"package digest mismatch for {path}")

    wheel = PACKAGE_DIR / "capturepkg-1.0.0-py3-none-any.whl"
    with zipfile.ZipFile(wheel) as archive:
        assert_true("capturepkg/__init__.py" in archive.namelist(), "wheel module missing")
        assert_true("capturepkg-1.0.0.dist-info/METADATA" in archive.namelist(), "wheel metadata missing")
        records = archive.read("capturepkg-1.0.0.dist-info/RECORD").decode().splitlines()
        for record in records[:-1]:
            name, encoded, size = record.split(",")
            content = archive.read(name)
            assert_true(encoded.startswith("sha256="), f"wheel RECORD missing digest for {name}")
            expected = base64.urlsafe_b64encode(hashlib.sha256(content).digest()).rstrip(b"=").decode()
            assert_true(encoded == f"sha256={expected}" and int(size) == len(content), f"wheel RECORD mismatch for {name}")
    with tarfile.open(fileobj=gzip.GzipFile(PACKAGE_DIR / "capturepkg-1.0.0.tar.gz"), mode="r") as archive:
        assert_true("capturepkg-1.0.0/PKG-INFO" in archive.getnames(), "sdist metadata missing")

    rpm = (PACKAGE_DIR / "capturepkg-1.0.0-1.x86_64.rpm").read_bytes()
    assert_true(rpm[:4] == b"\xed\xab\xee\xdb", "RPM lead missing")
    assert_true(b"capturepkg" in rpm and b"x86_64" in rpm, "RPM identity missing")
    deb = (PACKAGE_DIR / "capturepkg_1.0.0-1_amd64.deb").read_bytes()
    members = ar_members(deb)
    assert_true(list(members) == ["debian-binary", "control.tar.gz", "data.tar.gz"], "DEB members changed")
    with tarfile.open(fileobj=gzip.GzipFile(fileobj=io.BytesIO(members["control.tar.gz"])), mode="r") as archive:
        control = archive.extractfile("control").read()
        assert_true(b"Package: capturepkg" in control and b"Architecture: amd64" in control, "DEB control metadata missing")
    with tarfile.open(fileobj=gzip.GzipFile(fileobj=io.BytesIO(members["data.tar.gz"])), mode="r") as archive:
        assert_true("./usr/share/doc/capturepkg/README" in archive.getnames(), "DEB data missing")
    pacman = (PACKAGE_DIR / "capturepkg-1.0.0-1-x86_64.pkg.tar.zst").read_bytes()
    with tarfile.open(fileobj=io.BytesIO(zstd_raw_payload(pacman)), mode="r") as archive:
        assert_true(".PKGINFO" in archive.getnames(), "pacman package metadata missing")
    pacman_db = (ROOT / "metadata" / "capture.db").read_bytes()
    with tarfile.open(fileobj=io.BytesIO(zstd_raw_payload(pacman_db)), mode="r") as archive:
        assert_true("capturepkg-1.0.0-1/desc" in archive.getnames(), "pacman database metadata missing")
    assert_true((ROOT / "metadata" / "Packages.gz").read_bytes().startswith(b"\x1f\x8b"), "APT index is not gzip")
    assert_true(b"primary.xml.gz" in (ROOT / "metadata" / "repomd.xml").read_bytes(), "RPM metadata missing primary location")

    print(f"verified {len(blob_by_digest)} content-addressed blobs, {len(route_map['routes'])} routes, and five package formats")


if __name__ == "__main__":
    main()

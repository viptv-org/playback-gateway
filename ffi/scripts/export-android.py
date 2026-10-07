#!/usr/bin/env python3
"""Export checked native build products and their complete dependency notices."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from urllib.request import Request, urlopen

ABIS = {
    "armeabi-v7a": "armv7-linux-androideabi",
    "arm64-v8a": "aarch64-linux-android",
    "x86_64": "x86_64-linux-android",
}
BINDING = "ffi/generated/kotlin/uniffi/playback_gateway_ffi/playback_gateway_ffi.kt"
SYSTEM_LIBRARIES = {"libc.so", "libdl.so", "libm.so", "liblog.so", "libandroid.so"}
NOTICE_NAME = re.compile(r"^(?:license|licence|copying|copyright|notice|authors)(?:[._-]|$)", re.IGNORECASE)
UPSTREAM_NOTICES = {}


def upstream_notices(package, root):
    """Recover omitted workspace-root notices from a crate's recorded Git SHA."""
    vcs_path = root / ".cargo_vcs_info.json"
    if not vcs_path.is_file():
        raise RuntimeError("Dependency without notice text has no immutable upstream provenance")
    vcs = json.loads(vcs_path.read_text())
    revision = vcs.get("git", {}).get("sha1", "")
    repository = package.get("repository")
    # These two rqbit registry subcrates omit repository metadata. Their
    # published VCS paths and sources belong to the adopted rqbit repository.
    rqbit_paths = {"librqbit-buffers": "crates/buffers", "librqbit-clone-to-owned": "crates/clone_to_owned"}
    if repository is None and rqbit_paths.get(package["name"]) == vcs.get("path_in_vcs"):
        repository = "https://github.com/ikatson/rqbit"
    match = re.fullmatch(r"https://github.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(?:\.git)?/?", repository or "")
    if not match or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise RuntimeError("Dependency without notices requires reviewed immutable upstream provenance")
    owner, repo = match.groups()
    key = (owner, repo, revision)
    if key not in UPSTREAM_NOTICES:
        def fetch(url):
            request = Request(url, headers={"User-Agent": "playback-gateway-native-notice-inventory/1"})
            with urlopen(request, timeout=30) as response:
                data = response.read(4 * 1024 * 1024 + 1)
                if len(data) > 4 * 1024 * 1024:
                    raise RuntimeError("Oversized upstream notice response")
                return data
        commit = json.loads(fetch(f"https://api.github.com/repos/{owner}/{repo}/git/commits/{revision}"))
        if commit.get("sha") != revision:
            raise RuntimeError("Unverified upstream notice source commit")
        tree_sha = commit["tree"]["sha"]
        tree = json.loads(fetch(f"https://api.github.com/repos/{owner}/{repo}/git/trees/{tree_sha}"))
        if tree.get("sha") != tree_sha or tree.get("truncated"):
            raise RuntimeError("Unverified upstream notice source tree")
        texts = []
        for entry in sorted(tree["tree"], key=lambda item: item["path"]):
            if entry["type"] != "blob" or entry["mode"] != "100644" or not NOTICE_NAME.match(entry["path"]):
                continue
            url = f"https://raw.githubusercontent.com/{owner}/{repo}/{revision}/{entry['path']}"
            data = fetch(url)
            # Bind fetched bytes to the upstream Git tree's blob, in addition to
            # the immutable URL and SHA256 recorded in the exported inventory.
            blob = hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()
            if blob != entry["sha"]:
                raise RuntimeError("Upstream notice Git blob checksum mismatch")
            texts.append(("upstream/" + entry["path"], url, data))
        if not texts:
            raise RuntimeError("No preserved license text at the dependency's immutable upstream source")
        UPSTREAM_NOTICES[key] = texts
    return UPSTREAM_NOTICES[key]


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def inventory():
    metadata = json.loads(command("cargo", "metadata", "--locked", "--format-version", "1"))
    packages = {(p["name"], p["version"]): p for p in metadata["packages"]}
    selected = {}
    graphs = {}
    for abi, target in ABIS.items():
        graph = command("cargo", "tree", "--locked", "-p", "playback-gateway-ffi",
                        "--no-default-features", "--features", "torrent", "--target", target,
                        "--edges", "normal,build", "--prefix", "none", "--format", "{p}")
        graphs[abi] = graph + "\n"
        for line in graph.splitlines():
            match = re.match(r"^(\S+) v(\S+)", line)
            if not match:
                raise RuntimeError("Unrecognized dependency tree entry")
            key = match.groups()
            selected.setdefault(key, set()).add(abi)
    notices = []
    entries = []
    for key, abis in sorted(selected.items()):
        package = packages[key]
        root = Path(package["manifest_path"]).parent
        # Cargo package license metadata alone omits notices and bundled C licenses.
        # Preserve matching files throughout the actual package source, with bounded
        # regular-file reads and no local source paths in the exported inventory.
        files = []
        for path in sorted(root.rglob("*")):
            relative = path.relative_to(root)
            if any(part in {"target", ".git", ".scratch"} for part in relative.parts):
                continue
            if not NOTICE_NAME.match(path.name):
                continue
            if path.is_symlink() or not path.is_file():
                continue
            data = path.read_bytes()
            if len(data) > 4 * 1024 * 1024:
                raise RuntimeError("Oversized dependency notice")
            try:
                content = data.decode("utf-8")
            except UnicodeError as error:
                raise RuntimeError("Dependency notice is not UTF-8") from error
            files.append({"path": relative.as_posix(), "sha256": sha256(data)})
            notices.append(f"===== {key[0]} {key[1]} / {relative.as_posix()} =====\n{content}\n")
        if package.get("license_file"):
            relative = package["license_file"]
            if relative not in [item["path"] for item in files]:
                path = root / relative
                data = path.read_bytes()
                files.append({"path": relative, "sha256": sha256(data)})
                notices.append(f"===== {key[0]} {key[1]} / {relative} =====\n{data.decode('utf-8')}\n")
        if not package.get("license") and not package.get("license_file"):
            raise RuntimeError(f"Dependency lacks license metadata: {key[0]} {key[1]}")
        if not files:
            # Local gateway crates inherit the repository license and attribution.
            if package["source"] is None:
                inherited = [Path("third_party/rqbit/LICENSE"), Path("third_party/rqbit/LICENSE-APACHE-2.0.txt")] if root.is_relative_to(Path("third_party/rqbit").resolve()) else [Path("LICENSE")]
                for path in inherited:
                    data = path.read_bytes()
                    relative = "repository/" + path.as_posix()
                    files.append({"path": relative, "sha256": sha256(data)})
                    notices.append(f"===== {key[0]} {key[1]} / {relative} =====\n{data.decode('utf-8')}\n")
            else:
                for relative, url, data in upstream_notices(package, root):
                    files.append({"path": relative, "sha256": sha256(data), "url": url})
                    notices.append(f"===== {key[0]} {key[1]} / {relative} =====\n{data.decode('utf-8')}\n")
        source = package["source"]
        entries.append({"name": key[0], "version": key[1], "source": source or "gateway-source-revision",
                        "license": package.get("license"), "license_file": package.get("license_file"), "authors": package["authors"],
                        "abis": sorted(abis), "notices": files})
    if any(p["name"] in {"unrar-rs", "axum"} for p in entries):
        raise RuntimeError("Native dependency inventory contains archive/control implementation")
    return entries, "\n".join(notices), graphs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("revision", help="Expected full immutable gateway source revision")
    args = parser.parse_args()
    os.chdir(Path(__file__).resolve().parents[2])
    if not re.fullmatch(r"[0-9a-f]{40}", args.revision) or command("git", "rev-parse", "HEAD") != args.revision:
        raise RuntimeError("Expected immutable source revision does not match HEAD")
    if command("git", "status", "--porcelain", "--untracked-files=all"):
        raise RuntimeError("Artifact export requires a clean committed source tree")
    command("bash", "ffi/scripts/check-native-deps.sh")
    if args.destination.exists():
        raise RuntimeError("Use a fresh artifact destination")
    ndk = Path(os.environ["ANDROID_NDK_HOME"])
    host = "windows-x86_64" if os.name == "nt" else "darwin-x86_64" if os.uname().sysname == "Darwin" else "linux-x86_64"
    readelf = str(ndk / "toolchains/llvm/prebuilt" / host / "bin/llvm-readelf")
    dependencies, notices, graphs = inventory()
    proof = json.loads(Path("ffi/generated/android/build.json").read_text())
    if proof["revision"] != args.revision or proof["min_sdk"] != 24 or proof["abis"] != ABIS:
        raise RuntimeError("Native build source provenance does not match the immutable source revision")
    payload = {"LICENSE": Path("LICENSE").read_bytes(), "PROVENANCE.md": Path("PROVENANCE.md").read_bytes(),
               BINDING: Path(BINDING).read_bytes()}
    elf = {}
    for abi in ABIS:
        path = f"ffi/generated/android/jniLibs/{abi}/libplayback_gateway_ffi.so"
        header = command(readelf, "-hW", path)
        segments = command(readelf, "-lW", path)
        dynamic = command(readelf, "-dW", path)
        needed = re.findall(r"\(NEEDED\).*\[([^\]]+)\]", dynamic)
        if not needed or not set(needed).issubset(SYSTEM_LIBRARIES):
            raise RuntimeError("Native library requires an unverified non-system dynamic dependency")
        if "\"test-network-policy\"" in command("cargo", "tree", "--locked", "-p", "playback-gateway-ffi",
                                               "--no-default-features", "--features", "torrent", "--target", ABIS[abi],
                                               "--edges", "features", "--prefix", "none"):
            raise RuntimeError("Native fixture policy enabled")
        elf[abi] = {"header": header, "program_headers": segments, "dynamic": dynamic, "needed": needed}
        payload[path] = Path(path).read_bytes()
        if proof["files"].get(path) != sha256(payload[path]):
            raise RuntimeError("Native build provenance checksum mismatch")
    # Cargo.lock checksums pin every registry source; local rqbit source is pinned
    # by this same commit. This report is an inventory, never license clearance.
    report = {
        "version": 1, "revision": args.revision, "min_sdk": 24, "abis": ABIS,
        "cargo_lock_sha256": sha256(Path("Cargo.lock").read_bytes()),
        "rustc": command("rustc", "--version"), "cargo_ndk": command("cargo", "ndk", "--version"),
        "ndk": command(readelf, "--version"), "ndk_source_properties": (ndk / "source.properties").read_text(),
        "features": ["torrent"], "archive_excluded": True, "distribution_cleared": False,
        "distribution_note": "GPL-2.0-only gateway code and Apache-2.0 dependencies require separate distribution review. Archive exclusion and notice preservation do not establish license compatibility.",
        "dependencies": dependencies, "elf": elf, "build": proof,
    }
    payload["THIRD_PARTY/native-dependencies.json"] = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode()
    payload["THIRD_PARTY/native-NOTICES.txt"] = notices.encode()
    payload["THIRD_PARTY/rqbit-UPSTREAM.json"] = Path("third_party/rqbit/UPSTREAM.json").read_bytes()
    for abi, graph in graphs.items():
        # Cargo tree paths belong only in private build logs. Replace them with
        # stable local source labels in the exported dependency inventory.
        graph = re.sub(r" \([^)]*[/\\][^)]*\)", " (gateway-source-revision)", graph)
        payload[f"THIRD_PARTY/native-{abi}-dependencies.txt"] = graph.encode()
    payload["THIRD_PARTY/native-SHA256SUMS.txt"] = "".join(
        f"{sha256(data)}  {path}\n" for path, data in sorted(payload.items())).encode()
    manifest = {"version": 1, "repository": "viptv-org/playback-gateway", "revision": args.revision,
                "files": {path: sha256(data) for path, data in sorted(payload.items())}}
    for path, data in payload.items():
        dest = args.destination / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
    (args.destination / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Exported {len(payload)} checked artifact files from {args.revision}; {len(dependencies)} dependency packages; distribution and device qualification remain separate.")


if __name__ == "__main__":
    main()

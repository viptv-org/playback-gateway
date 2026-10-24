#!/usr/bin/env python3
"""Run exact current-source real-media tests against a specific candidate image."""

import argparse
import json
from pathlib import Path
import subprocess
import uuid
from corpus import digest, write_json

ENGINE_TESTS = [
    "tests::hdr::real_hdr_resize_linear_reference",
    "tests::hdr::real_hdr_resize_dimensions_and_sdr_tags",
    "tests::hdr::real_hdr10_file_is_delivered_as_original_to_a_webcodecs_client",
    "tests::hdr::real_common_format_conversion",
]
CONTROL_TESTS = [
    "workers::header_tests::real_header_media_uses_owned_capability_without_child_credentials_and_decodes",
    "workers::header_tests::real_native_mp4_prefers_owned_seekable_original_and_falls_back_for_other_codecs",
    "workers::tls_tests::real_shared_https_and_http_https_children_verify_trust_and_hostnames",
]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--engine-test-binary", type=Path, required=True)
    p.add_argument("--gateway-test-binary", type=Path, required=True)
    p.add_argument("--image", required=True)
    p.add_argument("--media-directory", default="/opt/jellyfin-ffmpeg/bin")
    p.add_argument(
        "--media-root",
        type=Path,
        help="optional staged media build containing bin/; mounted read-only at /qualified",
    )
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--tmpfs-mib", type=int, default=512)
    p.add_argument("--memory-mib", type=int, default=2048)
    p.add_argument("--suite", choices=["all", "engine", "control"], default="all")
    p.add_argument(
        "--test",
        action="append",
        help="exact test selectors for focused retries; requires --extended for extended selectors",
    )
    p.add_argument(
        "--extended",
        action="store_true",
        help="also retain existing file/live, selection, recovery and shared-ingest regressions",
    )
    args = p.parse_args()
    assert 128 <= args.tmpfs_mib <= 8192 and 512 <= args.memory_mib <= 8192
    args.output.mkdir(parents=True, exist_ok=True)
    if args.media_root:
        args.media_directory = "/qualified/bin"
    image_id = subprocess.check_output(
        ["sudo", "-n", "docker", "image", "inspect", "--format", "{{.Id}}", args.image],
        text=True,
    ).strip()
    results = []
    suites = []
    if args.suite in ("all", "engine"):
        suites.append((args.engine_test_binary, ENGINE_TESTS))
    if args.suite in ("all", "control"):
        suites.append((args.gateway_test_binary, CONTROL_TESTS))
    if args.extended:
        suites = [
            (
                binary,
                [
                    line.removesuffix(": test")
                    for line in subprocess.check_output(
                        [str(binary), "--list", "--ignored"], text=True
                    ).splitlines()
                    if line.endswith(": test")
                    and not any(
                        part in line
                        for part in (
                            "::vaapi::",
                            "::torrent_tests::",
                            "::archive_tests::",
                        )
                    )
                ],
            )
            for binary, _ in suites
        ]
    if args.test:
        available = {name for _, names in suites for name in names}
        assert set(args.test) <= available, "unknown selected test"
        suites = [
            (binary, [name for name in names if name in args.test])
            for binary, names in suites
        ]
    planned = sum(len(names) for _, names in suites)
    for binary, names in suites:
        # Validate names up front so stale artifacts cannot turn zero tests into a pass.
        listed = subprocess.check_output([str(binary), "--list"], text=True)
        for name in names:
            if name + ": test" not in listed:
                raise SystemExit("test selector missing: " + name)
            owned_name = "media-lab-test-" + uuid.uuid4().hex
            argv = [
                "sudo",
                "-n",
                "docker",
                "run",
                "--name",
                owned_name,
                "--rm",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--memory",
                str(args.memory_mib) + "m",
                "--cpus",
                "2",
                "--pids-limit",
                "256",
                "--tmpfs",
                "/tmp:rw,exec,nosuid,nodev,size=" + str(args.tmpfs_mib) + "m",
                "--mount",
                f"type=bind,source={binary.resolve()},target=/test,readonly",
                "--env",
                "PLAYBACK_TEST_FFMPEG=" + args.media_directory + "/ffmpeg",
                "--env",
                "PLAYBACK_TEST_FFPROBE=" + args.media_directory + "/ffprobe",
            ]
            if "tls_tests" in name or "security_tests" in name:
                argv += ["--security-opt", "seccomp=unconfined"]
                for capability in (
                    "SYS_ADMIN",
                    "SYS_CHROOT",
                    "SETUID",
                    "SETGID",
                    "DAC_OVERRIDE",
                    "CHOWN",
                ):
                    argv += ["--cap-add", capability]
            if args.media_root:
                argv += [
                    "--mount",
                    f"type=bind,source={args.media_root.resolve()},target=/qualified,readonly",
                ]
            argv += [
                "--entrypoint",
                "/test",
                image_id,
                name,
                "--ignored",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ]
            log = args.output / (name.replace("::", "-") + ".log")
            with log.open("wb") as output:
                try:
                    exit_code = subprocess.run(
                        argv, stdout=output, stderr=subprocess.STDOUT, timeout=240
                    ).returncode
                except subprocess.TimeoutExpired:
                    exit_code = 124
                    subprocess.run(
                        ["sudo", "-n", "docker", "rm", "--force", owned_name],
                        capture_output=True,
                        timeout=20,
                    )
            text = log.read_text()
            passed = (
                exit_code == 0
                and "test result: ok. 1 passed; 0 failed; 0 ignored;" in text
            )
            results.append(
                dict(
                    test=name,
                    status="passed" if passed else "failed",
                    exit_code=exit_code,
                    artifact_sha256=digest(binary),
                    log_sha256=digest(log),
                )
            )
            write_json(
                args.output / "summary.json",
                dict(
                    image_id=image_id,
                    media_directory=args.media_directory,
                    tmpfs_mib=args.tmpfs_mib,
                    memory_mib=args.memory_mib,
                    media_binary_sha256={
                        n: digest(args.media_root / "bin" / n)
                        for n in ("ffmpeg", "ffprobe")
                    }
                    if args.media_root
                    else None,
                    results=results,
                    suite=args.suite,
                    planned=planned,
                    complete=len(results) == planned,
                    passed=all(r["status"] == "passed" for r in results),
                ),
            )
            print(json.dumps(results[-1]), flush=True)
    raise SystemExit(any(r["status"] != "passed" for r in results))


if __name__ == "__main__":
    main()

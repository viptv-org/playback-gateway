#!/usr/bin/env python3
"""Run under an owned virtual display. Compare rendered KMP startup and seeks.

The reference is admitted only after the app is ready, so JVM/window startup
cannot secretly prewarm Stremio. Baseline and candidate use the same KMP host.
"""
import argparse
import atexit
import base64
import importlib.util
import json
import os
from pathlib import Path
import random
import signal
import subprocess
import time
import threading
import urllib.request

module = importlib.util.spec_from_file_location("screening", Path(__file__).with_name("benchmark-streaming.py"))
screen = importlib.util.module_from_spec(module); module.loader.exec_module(screen)


class Resources:
    """Sample only the owned JVM and its descendants; never capture commands/URLs."""
    def __init__(self, process, cache):
        self.process, self.cache = process, cache
        self.stop = threading.Event(); self.rows = []; self.cpu = {}; self.extra = []
        self.thread = threading.Thread(target=self.sample, daemon=True); self.thread.start()

    def sample(self):
        started = time.monotonic()
        while not self.stop.is_set():
            pending = [self.process.pid, *self.extra]; seen = set(); rss = 0; descriptors = 0
            while pending:
                pid = pending.pop()
                if pid in seen: continue
                seen.add(pid)
                try:
                    directory = Path('/proc') / str(pid)
                    stat = (directory/'stat').read_text().rsplit(')', 1)[1].split()
                    self.cpu[(pid, stat[19])] = (int(stat[11])+int(stat[12]))/os.sysconf('SC_CLK_TCK')
                    rss += int(stat[21])*os.sysconf('SC_PAGE_SIZE')
                    descriptors += len(list((directory/'fd').iterdir()))
                    for children in (directory/'task').glob('*/children'):
                        pending.extend(int(p) for p in children.read_text().split())
                except (OSError, ValueError): pass
            allocated = 0
            try:
                for file in self.cache.rglob('*'):
                    if file.is_file(): allocated += file.stat().st_blocks*512
            except OSError: pass
            self.rows.append(dict(seconds=time.monotonic()-started, rss_bytes=rss, descriptors=descriptors,
                                  cpu_seconds=sum(self.cpu.values()), allocated_cache_bytes=allocated))
            self.stop.wait(1)

    def close(self, output):
        self.stop.set(); self.thread.join(5)
        (output/'resources.json').write_text(json.dumps(self.rows))
        return dict(peak_rss_bytes=max((r['rss_bytes'] for r in self.rows), default=0),
                    cpu_seconds=sum(self.cpu.values()),
                    peak_cache_allocation_bytes=max((r['allocated_cache_bytes'] for r in self.rows), default=0))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--launcher", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True); parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--corpus", type=Path)
    parser.add_argument("--cases", nargs="+", default=screen.FILMS)
    parser.add_argument("--trials", type=int, default=20); parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--states", nargs="+", choices=["cold", "retained"], default=["cold", "retained"])
    parser.add_argument("--engines", nargs="+", choices=["baseline", "candidate", "stremio"], default=["baseline", "candidate", "stremio"])
    parser.add_argument("--sustained", type=int, default=0)
    parser.add_argument("--startup-only", action="store_true", help="Measure first rendered frame without seek trials")
    parser.add_argument("--candidate-grace", default="0")
    parser.add_argument("--candidate-profile", help="Test-only cold worker tuning profile")
    args = parser.parse_args(); os.umask(0o077)
    if args.startup_only and args.states != ['cold']:
        parser.error('--startup-only requires --states cold')
    if len(args.states) != len(set(args.states)) or ('cold' in args.states and args.states[0] != 'cold'):
        parser.error('cold must be the first state, and states must not repeat')
    def interrupted(*_): raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, interrupted)
    if not os.getenv("DISPLAY"): raise SystemExit("Use an owned managed virtual display")
    if args.output.exists(): raise SystemExit("Use a fresh evidence directory")
    args.output.mkdir(parents=True)
    # Route this job's audio to its own sink without changing the user's default.
    sink = "viptv_torrent_bench_" + str(os.getpid())
    audio_module = subprocess.check_output(["pactl", "load-module", "module-null-sink", "sink_name="+sink], text=True).strip()
    atexit.register(lambda: subprocess.run(["pactl", "unload-module", audio_module], check=False))
    (args.output / "owned-audio.json").write_text(json.dumps({"module":audio_module,"sink":sink}))
    alsa = args.output / "alsa.conf"; alsa.write_text("pcm.!default { type pulse }\nctl.!default { type pulse }\n")
    identity = {"trials": args.trials, "states": args.states, "seed": 20261022, "sustained_seconds": args.sustained,
                "startup_only": args.startup_only,
                "candidate_grace": args.candidate_grace,
                "candidate_profile": args.candidate_profile,
                "sha256": {name: screen.hashlib.sha256(path.read_bytes()).hexdigest() for name, path in
                    [("baseline", args.baseline), ("candidate", args.candidate), ("stremio", Path('/opt/stremio/server.js')), ("launcher", args.launcher)]}}
    (args.output / "run.json").write_text(json.dumps(identity, indent=2))
    cases = []
    for entry in json.loads(args.corpus.read_text())["sources"] if args.corpus else []:
        path = Path(entry["torrent"]).resolve()
        cases.append(dict(screen.decode_torrent(path.read_bytes()), torrent=path, mode="metainfo"))
    for name in [] if args.corpus else args.cases:
        if name not in screen.FILMS: raise ValueError("unknown case")
        path = args.output / (name+".torrent")
        with urllib.request.urlopen(f"https://webtorrent.io/torrents/{name}.torrent", timeout=30) as response: path.write_bytes(response.read(4<<20))
        cases.append(dict(screen.decode_torrent(path.read_bytes()), torrent=path))
    rng = random.Random(20261022); rows = []
    for trial in range(args.trials):
        indices = list(range(len(cases))); rng.shuffle(indices)
        for ordinal in indices:
            engines = args.engines.copy(); rng.shuffle(engines)
            for engine in engines:
                for state in args.states:
                    directory = args.output / f"trial-{trial}-{ordinal}-{engine}-{state}"; directory.mkdir()
                    result = directory / "result.jsonl"
                    cache = args.output / f"cache-{trial}-{ordinal}-{engine}"
                    if state == 'cold' and cache.exists():
                        raise RuntimeError('cold trial cache already exists')
                    case = cases[ordinal]
                    spec = {"result": str(result), "cache": str(cache), "timeout_ms": args.timeout*1000, "sustained_seconds": args.sustained}
                    spec['startup_only'] = args.startup_only
                    if engine == "stremio":
                        spec.update(reference_ready=str(directory / "reference-ready"), reference_reply=str(directory / "reference-reply.json"))
                    else:
                        spec.update(worker=str(args.baseline if engine == "baseline" else args.candidate),
                                    source={"magnet": "magnet:?xt=urn:btih:"+case["hash"], "file_index": case["index"], "trackers": case["trackers"]})
                        if case.get('mode')=='metainfo':
                            spec['source'].pop('magnet')
                            spec['source']['metainfo']=base64.b64encode(case['torrent'].read_bytes()).decode()
                    config = directory / "input.json"; config.write_text(json.dumps(spec))
                    env = dict(os.environ, VIPTV_TORRENT_BENCH_INPUT=str(config),
                               PULSE_SINK=sink, ALSA_CONFIG_PATH=str(alsa), SKIKO_RENDER_API="SOFTWARE",
                               TORRENT_STEAL_REQUEST_GRACE=args.candidate_grace if engine == "candidate" else "0")
                    if args.candidate_profile and engine == 'candidate':
                        env.update(TORRENT_COLD_PROFILE=args.candidate_profile, TORRENT_COLD_TRACE=str(directory/'cold-trace.jsonl'))
                    reference = None
                    row = {"trial": trial, "case": ordinal, "engine": engine, "state": state, "passed": False}
                    row['sustained_seconds'] = args.sustained
                    with (directory / "player.log").open("wb") as log:
                        process = subprocess.Popen([str(args.launcher)], env=env, stdout=log, stderr=log)
                        resources = Resources(process, cache)
                        try:
                            if engine == "stremio":
                                deadline = time.monotonic()+args.timeout
                                while not Path(spec["reference_ready"]).is_file():
                                    if process.poll() is not None or time.monotonic()>deadline: raise TimeoutError("player_not_ready")
                                    time.sleep(.02)
                                reference = screen.Engine("stremio", ["/opt/stremio/node", "/opt/stremio/server.js"], directory, cache, args.timeout)
                                resources.extra.append(reference.process.pid)
                                url = reference.prepare(case)
                                temporary = directory / "reply.tmp"; temporary.write_text(json.dumps({"url": url})); temporary.rename(spec["reference_reply"])
                            deadline = time.monotonic()+args.timeout*3+args.sustained+30
                            while process.poll() is None and not result.is_file():
                                if time.monotonic()>deadline: raise TimeoutError("player_timeout")
                                time.sleep(.05)
                            if result.is_file(): row.update(json.loads(result.read_text().splitlines()[-1]))
                            process.wait(10)
                            if process.returncode != 0 or not result.is_file(): raise RuntimeError("player_exited")
                            row["clean_shutdown"] = True
                        except Exception as error:
                            row["error_type"] = type(error).__name__; row["passed"] = False
                            row["clean_shutdown"] = False
                        finally:
                            if process.poll() is None:
                                process.terminate()
                                try: process.wait(5)
                                except subprocess.TimeoutExpired: process.kill(); process.wait()
                            if reference: reference.close()
                            row.update(resources.close(directory))
                    rows.append(row)
                    with (args.output / "results.jsonl").open("a") as file: file.write(json.dumps(row)+"\n")
                    report = screen.summary(rows, args.trials)
                    report.update(measurement="KMP rendered images on owned virtual display", qualification=False,
                                  reason="Requires per-case parity gates and sustained playback, plus platform qualification")
                    if args.startup_only:
                        report['reason'] = 'Cold-start qualification requires at least 20 paired cold trials per case; warm and seek results do not count'
                    report['performance_gate'] = screen.cold_start_gate(rows) if args.startup_only else screen.performance_gate(rows)
                    if args.startup_only:
                        report['cold_start_qualification'] = report['performance_gate']['passed']
                    (args.output / "summary.json").write_text(json.dumps(report, indent=2))
                    print(json.dumps({k:v for k,v in row.items() if k != "player_samples"}), flush=True)


if __name__ == "__main__": main()

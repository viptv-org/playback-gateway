#!/usr/bin/env python3
"""Serialized, paired decoder screening against an unmodified Stremio server.

This measures decoded frames, not app presentation. It cannot qualify a release.
Each engine gets its own cache. Failed trials remain in the JSONL and summary.
Use --corpus for controlled/private inputs; source identities never enter results.
"""
import argparse
import base64
from collections import defaultdict
import hashlib
import json
import math
import os
from pathlib import Path
import queue
import random
import socket
import statistics
import subprocess
import threading
import time
import urllib.parse
import urllib.request

FILMS = ["big-buck-bunny", "sintel", "tears-of-steel", "cosmos-laundromat"]


def decode_torrent(raw):
    offset = 0
    info_raw = None

    def value():
        nonlocal offset, info_raw
        token = raw[offset:offset+1]
        if token == b"i":
            end = raw.index(b"e", offset); n = int(raw[offset+1:end]); offset = end+1; return n
        if token in (b"l", b"d"):
            offset += 1; result = [] if token == b"l" else {}
            while raw[offset:offset+1] != b"e":
                if token == b"l": result.append(value())
                else:
                    key = value(); start = offset; result[key] = value()
                    if key == b"info": info_raw = raw[start:offset]
            offset += 1; return result
        end = raw.index(b":", offset); size = int(raw[offset:end]); offset = end+1
        data = raw[offset:offset+size]; offset += size; return data
    metadata = value()
    if offset != len(raw) or info_raw is None: raise ValueError("invalid metainfo")
    info = metadata[b"info"]
    files = info.get(b"files", [{b"length": info.get(b"length"), b"path": [info[b"name"]]}])
    index = max(range(len(files)), key=lambda i: files[i][b"length"])
    trackers = [url.decode() for tier in metadata.get(b"announce-list", []) for url in tier]
    if metadata.get(b"announce"): trackers.append(metadata[b"announce"].decode())
    trackers = list(dict.fromkeys(t for t in trackers if t.startswith(("udp://", "http://", "https://"))))
    return {"hash": hashlib.sha1(info_raw).hexdigest(), "index": index, "trackers": trackers,
            "piece_bytes": info[b"piece length"], "file_bytes": files[index][b"length"]}


def http(url, body=None, timeout=10):
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


class Engine:
    def __init__(self, name, command, directory, cache, timeout):
        self.name, self.directory, self.timeout = name, directory, timeout
        self.lock = threading.Lock(); self.lines = queue.Queue(); self.handle = None
        self.stop = threading.Event(); self.trace = []; self.process = None
        directory.mkdir(parents=True, exist_ok=True); cache.mkdir(parents=True, exist_ok=True)
        self.log = (directory / "engine.log").open("wb")
        self.launch_start = time.monotonic()
        try:
            if name == "stremio":
                for port in (11470, 12470):
                    with socket.socket() as probe:
                        if probe.connect_ex(("127.0.0.1", port)) == 0: raise RuntimeError("reference_port_in_use")
                self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=self.log, stderr=self.log,
                    env=dict(os.environ, APP_PATH=str(cache), FFMPEG_BIN="/usr/bin/ffmpeg", FFPROBE_BIN="/usr/bin/ffprobe"))
                deadline = time.monotonic()+timeout
                while True:
                    if self.process.poll() is not None: raise RuntimeError("reference_exited")
                    try: self.settings = http("http://127.0.0.1:11470/settings"); break
                    except Exception:
                        if time.monotonic() > deadline: raise TimeoutError("reference_ready_timeout")
                        time.sleep(.05)
                (directory / 'reference-settings.json').write_text(json.dumps(self.settings, indent=2))
            else:
                self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
                def reader():
                    try:
                        for line in self.process.stdout: self.lines.put(json.loads(line))
                    except Exception: pass
                    finally: self.lines.put(None)
                threading.Thread(target=reader, daemon=True).start()
                ready = self.lines.get(timeout=timeout)
                if not ready or ready.get("ready") is not True: raise RuntimeError("worker_not_ready")
                self.call(op="open", config={"cache_dir": str(cache), "cache_bytes": 2<<30})
            self.launch_seconds = time.monotonic()-self.launch_start
            self.monitor = threading.Thread(target=self.sample, daemon=True); self.monitor.start()
        except BaseException:
            self.close(); raise

    def call(self, **body):
        with self.lock:
            self.process.stdin.write(json.dumps(dict(version=2, **body)).encode()+b"\n"); self.process.stdin.flush()
            reply = self.lines.get(timeout=min(self.timeout, 10))
            if not reply or not reply.get("ok"): raise RuntimeError("worker_control_failed")
            return reply

    def sample(self):
        renewed = time.monotonic()
        while not self.stop.wait(.25):
            sample = {"elapsed": time.monotonic()-self.launch_start}
            try:
                stat = Path(f"/proc/{self.process.pid}/stat").read_text().rsplit(")", 1)[1].split()
                sample.update(cpu_ticks=int(stat[11])+int(stat[12]), rss_bytes=int(stat[21])*os.sysconf("SC_PAGE_SIZE"))
                if self.handle:
                    reply = self.call(op="observe", handle=self.handle)
                    sample["progress"] = reply.get("progress")
                    if time.monotonic()-renewed > 15:
                        self.call(op="renew", handle=self.handle, authority_ms=60000); renewed=time.monotonic()
            except Exception: sample["sample_failed"] = True
            self.trace.append(sample)

    def prepare(self, case):
        if self.name == "stremio":
            if case.get("mode", "magnet") == "metainfo":
                stats = http("http://127.0.0.1:11470/create", {"from": str(case["torrent"])}, self.timeout)
            else:
                sources = ["dht:"+case["hash"]] + ["tracker:"+t for t in case["trackers"]]
                stats = http("http://127.0.0.1:11470/"+case["hash"]+"/create", {"peerSearch": {"min": 40, "max": 200, "sources": sources}}, self.timeout)
            if not stats.get("files"): raise RuntimeError("reference_metadata_unavailable")
            return f"http://127.0.0.1:11470/{case['hash']}/{case['index']}"
        source = {"file_index": case["index"], "trackers": case["trackers"]}
        if case.get("mode", "magnet") == "metainfo": source["metainfo"] = base64.b64encode(case["torrent"].read_bytes()).decode()
        else: source["magnet"] = "magnet:?xt=urn:btih:"+case["hash"]
        self.handle = self.call(op="prepare", source=source, authority_ms=60000)["handle"]
        deadline = time.monotonic()+self.timeout
        while time.monotonic() < deadline:
            reply = self.call(op="observe", handle=self.handle)
            if reply.get("media_url"): return reply["media_url"]
            if reply.get("progress", {}).get("error"): raise RuntimeError("worker_prepare_failed")
            time.sleep(.05)
        raise TimeoutError("metadata_timeout")

    def acknowledge(self):
        if self.handle: self.call(op="first_frame", handle=self.handle)

    def close(self):
        start = time.monotonic(); self.stop.set()
        monitor = getattr(self, "monitor", None)
        if monitor: monitor.join(12)
        if self.process is not None:
            if self.name != "stremio" and self.process.poll() is None:
                try:
                    if self.handle: self.call(op="close", handle=self.handle)
                    self.call(op="shutdown")
                    self.process.stdin.close(); self.process.wait(2)
                except Exception: pass
            forced = self.process.poll() is None
            if forced:
                self.process.terminate()
                try: self.process.wait(2)
                except subprocess.TimeoutExpired: self.process.kill(); self.process.wait()
            (self.directory / "trace.json").write_text(json.dumps(self.trace))
        else: forced = False
        self.log.close()
        return {"stop_seconds": time.monotonic()-start, "forced_stop": forced}


def frame(url, offset, timeout, log):
    # Identical decoder and settings, no warm-up/probe range before the timer.
    command = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2"]
    if offset: command += ["-ss", str(offset)]
    command += ["-i", url, "-map", "0:v:0", "-frames:v", "1", "-threads", "2", "-f", "framehash", "-"]
    start = time.monotonic()
    with log.open("wb") as errors:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=errors, timeout=timeout)
    if result.returncode or not any(line and not line.startswith(b"#") for line in result.stdout.splitlines()):
        raise RuntimeError("decoder_failed")
    return time.monotonic()-start


def summary(rows, repetitions):
    groups = defaultdict(list)
    for row in rows: groups[(row["case"], row["state"], row["engine"])].append(row)
    out = []
    for (case, state, engine), trials in sorted(groups.items()):
        cell = {"case": case, "state": state, "engine": engine, "trials": len(trials), "failures": sum(not r["passed"] for r in trials)}
        for metric in ("first_frame_seconds", "forward_seconds", "backward_seconds"):
            values = sorted(r.get(metric, math.inf) for r in trials)
            if values:
                median = statistics.median(values); p90 = values[math.ceil(.9*len(values))-1]
                cell[metric] = {"median": median if math.isfinite(median) else None,
                                "p90": p90 if math.isfinite(p90) else None}
        out.append(cell)
    return {"qualification": False, "measurement": "FFmpeg decoded frame; application presentation and stalls unqualified",
            "screening_only": repetitions < 20, "cells": out}


def performance_gate(rows):
    """No successful-only statistics, cross-source averaging, or tolerance margin."""
    report = summary(rows, 20)
    cells = {(c['case'], c['state'], c['engine']): c for c in report['cells']}
    decisions = []
    for case, state in sorted({(r['case'],r['state']) for r in rows}):
        candidate = cells.get((case,state,'candidate')); reference = cells.get((case,state,'stremio'))
        reasons = []
        if candidate is None or reference is None: reasons.append('missing_engine')
        else:
            if min(candidate['trials'], reference['trials']) < 20: reasons.append('fewer_than_20_trials')
            if candidate['trials'] != reference['trials']: reasons.append('unpaired_trials')
            if candidate['failures']==candidate['trials'] or reference['failures']==reference['trials']: reasons.append('no_successful_comparison')
            if candidate['failures']/candidate['trials'] > reference['failures']/reference['trials']: reasons.append('higher_failure_rate')
            for metric in ('first_frame_seconds','forward_seconds','backward_seconds'):
                for stat in ('median','p90'):
                    actual = candidate.get(metric,{}).get(stat); expected = reference.get(metric,{}).get(stat)
                    if actual is None or expected is None: reasons.append(metric+'_'+stat+'_unavailable')
                    elif actual>expected: reasons.append(metric+'_'+stat+'_slower')
            c_stalls=[r for r in rows if r['case']==case and r['state']==state and r['engine']=='candidate' and r.get('sustained_seconds',0)>=600]
            r_stalls=[r for r in rows if r['case']==case and r['state']==state and r['engine']=='stremio' and r.get('sustained_seconds',0)>=600]
            if not c_stalls or not r_stalls: reasons.append('ten_minute_playback_missing')
            elif any(not r['passed'] for r in c_stalls) or statistics.median(r.get('seconds_without_new_frames',math.inf) for r in c_stalls)>statistics.median(r.get('seconds_without_new_frames',math.inf) for r in r_stalls): reasons.append('worse_sustained_playback')
        decisions.append({'case':case,'state':state,'passed':not reasons,'reasons':reasons})
    return {'passed':bool(decisions) and all(d['passed'] for d in decisions),'cases':decisions}


def cold_start_gate(rows):
    """Cold-only acceptance: no warm-cache or seek gains can offset a cold loss."""
    cells = {(c['case'], c['state'], c['engine']): c for c in summary(rows, 20)['cells']}
    decisions = []
    for case in sorted({r['case'] for r in rows}):
        candidate = cells.get((case, 'cold', 'candidate')); reference = cells.get((case, 'cold', 'stremio'))
        reasons = []
        if candidate is None or reference is None: reasons.append('missing_cold_engine')
        else:
            paired = {engine: [r.get('trial') for r in rows if r['case'] == case
                               and r['state'] == 'cold' and r['engine'] == engine]
                      for engine in ('candidate', 'stremio')}
            if min(candidate['trials'], reference['trials']) < 20: reasons.append('fewer_than_20_trials')
            if (None in paired['candidate'] or None in paired['stremio']
                    or set(paired['candidate']) != set(paired['stremio'])
                    or any(len(ids) != len(set(ids)) for ids in paired.values())):
                reasons.append('unpaired_trials')
            if candidate['failures']==candidate['trials'] or reference['failures']==reference['trials']: reasons.append('no_successful_comparison')
            if candidate['failures']/candidate['trials'] > reference['failures']/reference['trials']: reasons.append('higher_failure_rate')
            for stat in ('median','p90'):
                actual = candidate.get('first_frame_seconds',{}).get(stat)
                expected = reference.get('first_frame_seconds',{}).get(stat)
                if actual is None or expected is None: reasons.append(stat+'_unavailable')
                elif actual > expected: reasons.append(stat+'_slower')
        decisions.append({'case':case,'passed':not reasons,'reasons':reasons})
    return {'passed':bool(decisions) and all(d['passed'] for d in decisions),'cases':decisions}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True); parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--stremio", type=Path, default=Path("/opt/stremio/server.js"))
    parser.add_argument("--node", default="/opt/stremio/node")
    parser.add_argument("--output", type=Path, required=True); parser.add_argument("--corpus", type=Path)
    parser.add_argument("--cases", nargs="+", default=FILMS); parser.add_argument("--trials", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=90); parser.add_argument("--seed", type=int, default=20261022)
    parser.add_argument("--states", nargs="+", choices=["cold", "retained"], default=["cold", "retained"])
    parser.add_argument("--engines", nargs="+", choices=["baseline", "candidate", "stremio"], default=["baseline", "candidate", "stremio"])
    args = parser.parse_args(); os.umask(0o077)
    if args.output.exists(): raise SystemExit("Use a new output directory; evidence must not be overwritten")
    args.output.mkdir(parents=True)
    inputs = args.output / "inputs"; inputs.mkdir()
    commands = {"baseline": [str(args.baseline.resolve())], "candidate": [str(args.candidate.resolve())], "stremio": [args.node, str(args.stremio.resolve())]}
    identities = {key: hashlib.sha256(Path(command[-1]).read_bytes()).hexdigest() for key, command in commands.items()}
    (args.output / "run.json").write_text(json.dumps({"sha256": identities, "seed": args.seed, "trials": args.trials, "timeout": args.timeout, "states": args.states}, indent=2))
    cases = []
    if args.corpus:
        for entry in json.loads(args.corpus.read_text())["sources"]:
            path = Path(entry["torrent"]).resolve()
            cases.append(dict(decode_torrent(path.read_bytes()), torrent=path, **{k:v for k,v in entry.items() if k != "torrent"}))
    else:
        for name in args.cases:
            if name not in FILMS: raise ValueError("unknown public case")
            path = inputs / (name+".torrent")
            with urllib.request.urlopen(f"https://webtorrent.io/torrents/{name}.torrent", timeout=30) as response: path.write_bytes(response.read(4<<20))
            cases.append(dict(decode_torrent(path.read_bytes()), torrent=path))
    rows = []; rng = random.Random(args.seed)
    for trial in range(args.trials):
        order = list(range(len(cases))); rng.shuffle(order)
        for ordinal in order:
            engines = args.engines.copy(); rng.shuffle(engines)
            for engine in engines:
                cache = args.output / f"cache-{trial}-{ordinal}-{engine}"
                for state in args.states:
                    directory = args.output / f"trial-{trial}-{ordinal}-{engine}-{state}"
                    result = {"trial": trial, "case": ordinal, "engine": engine, "state": state, "passed": False}
                    instance = None
                    try:
                        instance = Engine(engine, commands[engine], directory, cache, args.timeout)
                        result["launch_seconds"] = instance.launch_seconds
                        start = time.monotonic(); url = instance.prepare(cases[ordinal])
                        result["endpoint_seconds"] = time.monotonic()-start
                        frame(url, 0, args.timeout, directory / "startup.log")
                        result["first_frame_seconds"] = time.monotonic()-start
                        instance.acknowledge()
                        for label, offset in [("forward", 120), ("backward", 10)]:
                            result[label+"_seconds"] = frame(url, offset, args.timeout, directory / (label+".log"))
                        result["passed"] = True
                    except Exception as error:
                        result["error_type"] = type(error).__name__
                    finally:
                        if instance: result.update(instance.close())
                    rows.append(result)
                    with (args.output / "results.jsonl").open("a") as file: file.write(json.dumps(result)+"\n")
                    (args.output / "summary.json").write_text(json.dumps(summary(rows, args.trials), indent=2))
                    print(json.dumps(result), flush=True)


if __name__ == "__main__": main()

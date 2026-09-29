#!/usr/bin/env python3
"""Repeatable local screening suite with raw results and resource observations."""
import argparse
import datetime
import json
from pathlib import Path
import subprocess
import sys
import threading
import time

from lab import COMPOSE, ROOT, environment, start
from model import PROFILES


def measure(command, backend, profile, path, *extra):
    args = [sys.executable, str(Path(__file__).with_name("bench.py")), command,
            "--backend",backend,"--profile",profile,"--output",str(path),*extra]
    result = subprocess.run(args,env=environment(),capture_output=True,text=True)
    if result.returncode:
        path.with_suffix(".error.txt").write_text(result.stdout+result.stderr)
        raise RuntimeError(f"{backend}/{profile}/{command} failed: {result.stderr[-3000:]}")
    return json.loads(path.read_text())


def resource_sampler(backend, destination, stop):
    name = f"githearth-metadata-bench-{backend}-1"
    proc = subprocess.Popen(["docker","stats","--format","{{json .}}",name],stdout=subprocess.PIPE,text=True)
    def consume():
        with destination.open("w") as output:
            for line in proc.stdout:
                try:
                    # Docker emits an ANSI cursor prefix even when stdout is a pipe.
                    sample = {"utc":datetime.datetime.now(datetime.timezone.utc).isoformat(),"sample":json.loads(line[line.index('{'):])}
                    output.write(json.dumps(sample)+"\n")
                    output.flush()
                except (json.JSONDecodeError, ValueError):
                    continue
    thread = threading.Thread(target=consume,daemon=True)
    thread.start()
    stop.wait()
    proc.terminate()
    proc.wait(timeout=10)
    thread.join(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--seconds",type=float,default=6)
    parser.add_argument("--repetitions",type=int,default=3)
    parser.add_argument("--profiles",nargs="+",choices=PROFILES,default=["solo","small","growth","enterprise"])
    parser.add_argument("--backends",nargs="+",choices=["postgres","foundationdb"],default=["postgres","foundationdb"])
    args = parser.parse_args()
    if args.seconds <= 0 or args.repetitions < 1:
        parser.error("seconds must be positive and repetitions must be at least one")
    if args.output.exists():
        parser.error("output directory already exists; choose a new run directory")
    args.output.mkdir(parents=True)
    manifest = {"started_utc":datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "command":sys.argv,"db_cpu_limit":4,"db_memory_limit_gib":3,
                "fdb_processes":4,"failure_domains":1,"warmup_seconds":2,"status":"running","operation_errors":0}
    (args.output/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")
    try:
        for profile_index,profile in enumerate(args.profiles):
            backends = ["postgres","foundationdb"] if profile_index%2==0 else ["foundationdb","postgres"]
            backends = [backend for backend in backends if backend in args.backends]
            for backend in backends:
                print(f"START {backend} {profile}",flush=True)
                start(backend)
                base = args.output/f"{backend}-{profile}"
                base.mkdir()
                stop = threading.Event()
                sampler = threading.Thread(target=resource_sampler,args=(backend,base/"resources.jsonl",stop),daemon=True)
                sampler.start()
                try:
                    for rep in range(args.repetitions):
                        measure("seed",backend,profile,base/f"seed-{rep}.json")
                        if rep==0:
                            measure("check",backend,profile,base/"correctness.json")
                        order = [1,8,32]
                        order = order[rep%3:]+order[:rep%3]
                        for workers in order:
                            path = base/f"mixed-w{workers}-r{rep}.json"
                            result = measure("run",backend,profile,path,"--workers",str(workers),"--seconds",str(args.seconds),"--warmup","2")
                            print(f"{backend} {profile} workers={workers} rep={rep}: {result['successful_operations_per_second']} ops/s p99={result['latency']['p99_ms']}ms errors={sum(result['errors'].values())}",flush=True)
                            manifest["operation_errors"] += sum(result["errors"].values())
                        if profile in ("small","enterprise"):
                            result = measure("run",backend,profile,base/f"hot-w8-r{rep}.json","--scenario","hot","--workers","8","--seconds",str(args.seconds),"--warmup","2")
                            print(f"{backend} {profile} HOT rep={rep}: {result['successful_operations_per_second']} ops/s p99={result['latency']['p99_ms']}ms errors={sum(result['errors'].values())}",flush=True)
                            manifest["operation_errors"] += sum(result["errors"].values())
                        measure("verify",backend,profile,base/f"invariants-{rep}.json")
                    measure("metadata",backend,profile,base/"metadata.json")
                    data_path = "/var/lib/postgresql" if backend=="postgres" else "/var/lib/foundationdb/data"
                    footprint = subprocess.run([*COMPOSE,"exec","-T",backend,"du","-sk",data_path],check=True,capture_output=True,text=True).stdout
                    (base/"volume-allocated-kib.txt").write_text(footprint)
                    if backend=="foundationdb":
                        status = subprocess.run([*COMPOSE,"exec","-T",backend,"fdbcli","-C","/config/fdb.cluster","--exec","status json"],check=True,capture_output=True,text=True).stdout
                        (base/"cluster-status.json").write_text(status)
                    measure("crash",backend,profile,base/"process-recovery.json")
                    measure("verify",backend,profile,base/"post-recovery-invariants.json")
                finally:
                    stop.set()
                    sampler.join(timeout=15)
        manifest["status"] = "completed"
    except BaseException as exc:
        manifest.update(status="failed",error=repr(exc))
        raise
    finally:
        subprocess.run([*COMPOSE,"stop"],check=True)
        manifest["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (args.output/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")


if __name__=="__main__":
    main()

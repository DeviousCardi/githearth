#!/usr/bin/env python3
"""Manage only the named, loopback-only GitHearth benchmark containers."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
COMPOSE = ["docker", "compose", "-f", str(Path(__file__).with_name("compose.yaml"))]


def command(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def compose(*args, **kwargs):
    return command([*COMPOSE, *args], **kwargs)


def fdb_cli(text):
    return compose("exec", "-T", "foundationdb", "fdbcli", "-C", "/config/fdb.cluster",
                   "--timeout", "10", "--exec", text, capture_output=True)


def start(backend):
    compose("stop")
    compose("up", "-d", backend)
    deadline = time.monotonic() + 60
    configured = False
    while time.monotonic() < deadline:
        try:
            if backend == "postgres":
                compose("exec", "-T", "postgres", "pg_isready", "-h", "127.0.0.1", "-p", "15432", "-U", "githearth_bench", capture_output=True)
                return
            status = json.loads(fdb_cli("status json").stdout)
            if status.get("client", {}).get("database_status", {}).get("available"):
                return
            if not configured:
                result = fdb_cli("configure new single ssd")
                print(result.stdout.strip())
                configured = True
        except (subprocess.CalledProcessError, json.JSONDecodeError):
            if backend == "foundationdb" and not configured:
                # An unconfigured cluster can time out while producing status.
                # `configure new` refuses to overwrite an existing database.
                configured = True
                result = fdb_cli("configure new single ssd")
                print(result.stdout.strip())
        time.sleep(0.5)
    raise RuntimeError(f"{backend} did not become ready within 60 seconds")


def environment():
    return {**os.environ, "PYTHONPATH": str(ROOT / ".bench/python"),
            "LD_LIBRARY_PATH": str(ROOT / ".bench/lib")}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("action", choices=["prepare", "start", "stop", "run", "status"])
    p.add_argument("args", nargs=argparse.REMAINDER)
    args = p.parse_args()
    if args.action == "prepare":
        (ROOT / ".bench/lib").mkdir(parents=True, exist_ok=True)
        command([sys.executable, "-m", "pip", "install", "--target", str(ROOT / ".bench/python"),
                 "-r", str(Path(__file__).with_name("requirements.txt"))])
        compose("pull")
        compose("create", "foundationdb")
        cid = compose("ps", "-a", "-q", "foundationdb", capture_output=True).stdout.strip()
        command(["docker", "cp", cid + ":/usr/lib/libfdb_c.so", str(ROOT / ".bench/lib/libfdb_c.so")])
    elif args.action == "start":
        if args.args not in (["postgres"], ["foundationdb"]):
            p.error("start requires postgres or foundationdb")
        start(args.args[0])
    elif args.action == "stop":
        compose("stop")
    elif args.action == "status":
        compose("ps", "-a")
    else:
        command([sys.executable, str(Path(__file__).with_name("bench.py")), *args.args], env=environment())


if __name__ == "__main__":
    main()

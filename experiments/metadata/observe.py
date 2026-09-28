#!/usr/bin/env python3
"""Capture timestamped Docker resource samples for a running local lab."""
import argparse
import datetime
import json
import subprocess


def main():
    p = argparse.ArgumentParser()
    p.add_argument("output")
    args = p.parse_args()
    command = ["docker","stats","--all","--format","{{json .}}",
               "githearth-metadata-bench-postgres-1","githearth-metadata-bench-foundationdb-1"]
    process = subprocess.Popen(command,stdout=subprocess.PIPE,text=True)
    try:
        with open(args.output,"w") as output:
            for line in process.stdout:
                try:
                    record = {"utc":datetime.datetime.now(datetime.timezone.utc).isoformat(),
                              "sample":json.loads(line[line.index('{'):])}
                except (json.JSONDecodeError,ValueError):
                    continue
                output.write(json.dumps(record)+"\n")
                output.flush()
    finally:
        process.terminate()
        process.wait(timeout=10)


if __name__=="__main__":
    main()

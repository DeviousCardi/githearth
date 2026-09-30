#!/usr/bin/env python3
"""Summarize a completed suite without dropping failed operations or slow runs."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median


def summarize(root):
    manifest = json.loads((root/"manifest.json").read_text())
    if manifest["status"] != "completed":
        raise ValueError("Only a completed suite may produce the comparison report")
    groups = defaultdict(list)
    for path in root.glob("*/*-w*-r*.json"):
        row = json.loads(path.read_text())
        groups[(row["profile"],row["scenario"],row["workers"],row["backend"])].append(row)
    if not groups:
        raise ValueError("No measured runs found")
    durations=sorted({r["duration_requested_seconds"] for rows in groups.values() for r in rows})
    warmups=sorted({r["warmup_seconds"] for rows in groups.values() for r in rows})
    repeat_counts=sorted({len(rows) for rows in groups.values()})
    profile_order = {name:n for n,name in enumerate(("solo","small","growth","enterprise"))}
    lines = ["# Local metadata screening results", "", "Status: preliminary single-host evidence; production qualification remains open. The implementation decision is recorded separately in ADR 0001.", "",
             "All numbers below describe the synthetic application workload, not registered-user capacity or raw database limits. Each row reports the median of repeated runs; p99 is the median of each run's successful-operation p99, not a pooled percentile. Failed requests are counted separately.", "",
             "## Repeated runs", "", "| Profile | Scenario | Clients | Backend | Runs | Median ops/s | Range ops/s | Median p99 ms | Errors |", "| --- | --- | ---: | --- | ---: | ---: | --- | ---: | ---: |"]
    aggregate = []
    for key,rows in sorted(groups.items(),key=lambda item:(profile_order[item[0][0]],item[0][1],item[0][2],item[0][3])):
        profile,scenario,workers,backend = key
        rates=[r["successful_operations_per_second"] for r in rows]
        p99s=[r["latency"]["p99_ms"] for r in rows]
        errors=sum(sum(r["errors"].values()) for r in rows)
        summary={"profile":profile,"scenario":scenario,"workers":workers,"backend":backend,"runs":len(rows),
                 "median_ops_per_second":round(median(rates),2),"min_ops_per_second":min(rates),"max_ops_per_second":max(rates),
                 "median_success_p99_ms":round(median(p99s),3),"errors":errors}
        aggregate.append(summary)
        lines.append(f"| {profile} | {scenario} | {workers} | {backend} | {len(rows)} | {median(rates):,.1f} | {min(rates):,.1f}–{max(rates):,.1f} | {median(p99s):.3f} | {errors} |")
    lines += ["", "## Process recovery", "", "The test first commits 100 unique markers, kills the benchmark database container, restarts it with the same volume, and reads every marker back. The host and disk remain intact. Times include restart and marker verification, not just database startup. This is not a power-loss, quorum, or disaster-recovery test.", "",
              "| Profile | Backend | Acknowledged markers | Lost markers | Restart and verify seconds |", "| --- | --- | ---: | ---: | ---: |"]
    for path in sorted(root.glob("*/process-recovery.json")):
        r=json.loads(path.read_text())
        profile=path.parent.name.removeprefix(r["backend"]+"-")
        lines.append(f"| {profile} | {r['backend']} | {r['acknowledged_markers']} | {r['lost_markers']} | {r['restart_and_verify_seconds']} |")
    lines += ["", "## Operation failures", ""]
    error_rows=[]
    for path in sorted(root.glob("*/*-w*-r*.json")):
        r=json.loads(path.read_text())
        for error,count in r["errors"].items():
            error_rows.append(f"- `{path.relative_to(root)}`: {count} × `{error}`; failed-request max {r.get('failed_latency',{}).get('max_ms')} ms.")
    lines += error_rows or ["No measured operation failures occurred in this suite."]
    lines += ["", "## Resource evidence and limits", "",
              "Each backend has a 4-CPU quota and 3 GiB memory limit. FoundationDB runs four server processes in one failure domain with single redundancy and the ssd-2 engine; PostgreSQL uses its normal server/backend process model with synchronous local commits. The clients share the host with the database.", "",
              "Each backend/profile directory contains timestamped `resources.jsonl`, allocated volume size, database configuration, correctness checks, and post-recovery invariant checks. Docker memory observations exclude some reclaimable cache and are not directly comparable to configured cache sizes. Allocated volume sizes include backend-specific logs and allocation behavior; they are not equivalent logical-data sizes.", "",
              f"Warmup seconds: {warmups}; requested measurement seconds: {durations}; repetitions per comparison row: {repeat_counts}. The suite rotates concurrency order and reseeds per repetition. These short, closed-loop runs expose gross differences and contention; they do not establish steady-state SLOs. Seed and verification work can affect cache and background cleanup. Reported ranges must be considered before interpreting small differences.", "",
              "The fixture uses simple, compressible text, basic memberships, and 100 issues per repository. A 100,000-account dataset is not 100,000 active sessions. Complex permission graphs, real repository distributions, lease expiry, outbox draining, multi-node latency, backups, online migrations, and working sets beyond cache are not qualified by this result.", "",
              "The comparison includes Python driver, serialization, query shape, secondary-index maintenance, and retry-policy costs. A Rust confirmation and equal-durability distributed test are required before attributing the result to a final production architecture.", ""]
    (root/"summary.json").write_text(json.dumps(aggregate,indent=2)+"\n")
    (root/"REPORT.md").write_text("\n".join(lines))


if __name__=="__main__":
    parser=argparse.ArgumentParser()
    parser.add_argument("results",type=Path)
    summarize(parser.parse_args().results)

#!/usr/bin/env python3
"""Closed-loop screening benchmark; not a production capacity certification."""
import argparse
import datetime
from collections import Counter, defaultdict
from concurrent.futures import ProcessPoolExecutor
import json
import math
import multiprocessing as mp
import os
from pathlib import Path
import platform
import random
import subprocess
import time
import uuid

from adapters import connect
from model import PROFILES, operation, user_for


def percentile(values, p):
    if not values:
        return None
    return round(sorted(values)[max(0, math.ceil(len(values) * p) - 1)] / 1_000_000, 3)


def summarize(values):
    return {"count":len(values), "p50_ms":percentile(values,0.50),
            "p95_ms":percentile(values,0.95), "p99_ms":percentile(values,0.99),
            "max_ms":round(max(values)/1_000_000,3) if values else None}


def worker(backend, profile_name, scenario, index, run_id, barrier, start_at, duration, warmup, output):
    try:
        db = connect(backend)
        rng = random.Random(20261027 + index)
        profile = PROFILES[profile_name]
        barrier.wait(timeout=60)
        barrier.wait(timeout=60)
        start = start_at.value
        time.sleep(max(0,start-time.monotonic()))
        measured_start, deadline = start + warmup, start + warmup + duration
        latencies, errors, results = defaultdict(list), Counter(), Counter()
        failed_latencies = []
        attempts = retries = 0
        seq = 0
        cpu_start = time.process_time()
        while time.monotonic() < deadline:
            seq += 1
            op = operation(rng,profile,f"{run_id}-{index:03}-{seq:09}",scenario)
            begin = time.monotonic()
            ns = time.perf_counter_ns()
            measured = begin >= measured_start
            try:
                result, retry_count = db.execute(op)
                if measured:
                    latencies[op["kind"]].append(time.perf_counter_ns()-ns)
                    retries += retry_count
                    results["empty_claims" if op["kind"]=="claim" and result is None else "completed"] += 1
            except Exception as exc:
                if measured:
                    errors[type(exc).__name__ + ": " + str(exc)[:160]] += 1
                    failed_latencies.append(time.perf_counter_ns()-ns)
            if measured:
                attempts += 1
        finished = time.monotonic()
        db.close()
        output.put({"latencies":dict(latencies), "failed_latencies":failed_latencies, "errors":dict(errors), "results":dict(results),
                    "attempts":attempts,"retries":retries,"finish":finished,
                    "client_cpu_seconds":time.process_time()-cpu_start})
    except BaseException as exc:
        barrier.abort()
        output.put({"fatal":repr(exc)})


def run(args):
    ctx = mp.get_context("spawn")
    barrier, start_at, output = ctx.Barrier(args.workers+1), ctx.Value("d",0), ctx.Queue()
    run_id = uuid.uuid4().hex
    processes = [ctx.Process(target=worker,args=(args.backend,args.profile,args.scenario,i,run_id,
                  barrier,start_at,args.seconds,args.warmup,output)) for i in range(args.workers)]
    for process in processes:
        process.start()
    try:
        barrier.wait(timeout=60)
        start_at.value = time.monotonic() + 0.2
        barrier.wait(timeout=60)
        reports = [output.get(timeout=args.seconds+args.warmup+60) for _ in processes]
    finally:
        for process in processes:
            process.join(timeout=1)
            if process.is_alive():
                process.terminate()
                process.join()
    if any("fatal" in r for r in reports):
        raise RuntimeError(reports)
    values, errors, outcomes = defaultdict(list), Counter(), Counter()
    for report in reports:
        for kind, timings in report["latencies"].items():
            values[kind].extend(timings)
        errors.update(report["errors"])
        outcomes.update(report["results"])
    elapsed = max(r["finish"] for r in reports) - (start_at.value+args.warmup)
    samples = [v for timings in values.values() for v in timings]
    result = {"schema":1,"backend":args.backend,"profile":args.profile,"dataset":PROFILES[args.profile].describe(),
              "scenario":args.scenario,"workers":args.workers,"duration_requested_seconds":args.seconds,
              "warmup_seconds":args.warmup,"elapsed_seconds":round(elapsed,3),"load_model":"closed-loop",
              "successful_operations_per_second":round(len(samples)/elapsed,2),
              "attempts":sum(r["attempts"] for r in reports),"retries_on_successful_operations":sum(r["retries"] for r in reports),
              "latency":summarize(samples),"failed_latency":summarize([n for r in reports for n in r["failed_latencies"]]),"operations":{k:summarize(v) for k,v in values.items()},
              "errors":dict(errors),"outcomes":dict(outcomes),
              "client_cpu_seconds_including_warmup":round(sum(r["client_cpu_seconds"] for r in reports),3),
              "host":{"logical_cpus":os.cpu_count(),"platform":platform.platform(),"python":platform.python_version()},
              "run_id":run_id}
    return result


def invoke(backend, op):
    db = connect(backend)
    try:
        return db.execute(op)[0]
    finally:
        db.close()


def correctness(backend, profile_name):
    profile = PROFILES[profile_name]
    db = connect(backend)
    prefix = "check-" + uuid.uuid4().hex
    op = {"kind":"comment","id":prefix,"tenant":1,"repo":1,"issue":1,"user":1}
    db.close()
    # Spawn, not fork: FoundationDB's network thread must not be inherited.
    with ProcessPoolExecutor(max_workers=8,mp_context=mp.get_context("spawn")) as pool:
        replies = list(pool.map(invoke,[backend]*16,[op]*16))
        claim_ops = [{"kind":"claim","id":f"{prefix}-parallel-{n}","tenant":1,"bucket":0} for n in range(16)]
        claimed = list(pool.map(invoke,[backend]*16,claim_ops))
        assert None not in claimed and len(set(claimed))==16
    assert set(replies) == {prefix}
    db = connect(backend)
    try:
        db.execute({**op,"issue":2})
    except ValueError:
        pass
    else:
        raise AssertionError("Idempotency mismatch accepted")
    page = db.execute({"kind":"list","id":prefix,"tenant":1,"repo":1,"user":1,"before":101})[0]
    expected = [i for i in range(100,0,-1) if i%4][:20]
    assert [r["id"] for r in page] == expected
    assert not db.execute({"kind":"permission","id":prefix,"tenant":1,"repo":1,"user":profile.users+1})[0]
    assert not db.execute({"kind":"permission","id":prefix,"tenant":1,"repo":profile.repos_per_tenant+1,"user":1})[0]
    if profile.tenants>1:
        assert not db.execute({"kind":"permission","id":prefix,"tenant":2,"repo":1,"user":1})[0]
    try:
        db.execute({**op,"id":prefix+"-denied","user":profile.users+1})
    except PermissionError:
        pass
    else:
        raise AssertionError("Unauthorized comment accepted")
    enq = {"kind":"enqueue","id":prefix+"-job","tenant":1,"bucket":0}
    assert db.execute(enq)[0] == db.execute(enq)[0]
    claim = {"kind":"claim","id":prefix+"-claim","tenant":1,"bucket":0}
    assert db.execute(claim)[0] == db.execute(claim)[0]
    invariants = db.verify()
    db.close()
    return {"backend":backend,"profile":profile_name,"checks":["concurrent_duplicate_comment","exclusive_concurrent_job_claims","idempotency_payload_binding","pagination","unknown_user_denied","unknown_repo_denied","unauthorized_write_denied","duplicate_enqueue","duplicate_claim","event_and_counter_invariants"],"invariants":invariants}


def crash_check(backend):
    compose = ["docker","compose","-f",str(Path(__file__).with_name("compose.yaml"))]
    db = connect(backend)
    markers = {f"crash-{uuid.uuid4().hex}":uuid.uuid4().hex for _ in range(100)}
    for key,value in markers.items():
        assert db.probe(key,value) == value
    db.close()
    subprocess.run([*compose,"kill","-s","SIGKILL",backend],check=True,capture_output=True)
    started = time.monotonic()
    subprocess.run([*compose,"start",backend],check=True,capture_output=True)
    deadline = started+60
    while True:
        db = None
        try:
            db = connect(backend)
            for key,value in markers.items():
                assert db.probe(key) == value, "An acknowledged marker was lost"
            break
        except AssertionError:
            raise
        except Exception:
            if time.monotonic()>=deadline:
                raise
            time.sleep(0.25)
        finally:
            if db:
                db.close()
    return {"backend":backend,"acknowledged_markers":len(markers),"lost_markers":0,
            "restart_and_verify_seconds":round(time.monotonic()-started,3),
            "fault":"database process SIGKILL; host and disk remain intact"}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("command",choices=["seed","run","check","verify","crash","metadata"])
    p.add_argument("--backend",choices=["postgres","foundationdb"],required=True)
    p.add_argument("--profile",choices=PROFILES,default="small")
    p.add_argument("--scenario",choices=["mixed","hot"],default="mixed")
    p.add_argument("--workers",type=int,default=8)
    p.add_argument("--seconds",type=float,default=10)
    p.add_argument("--warmup",type=float,default=3)
    p.add_argument("--output",type=Path)
    args = p.parse_args()
    if not 1<=args.workers<=64 or args.seconds<=0 or args.warmup<0:
        p.error("workers must be 1..64; seconds > 0; warmup >= 0")
    begin = time.monotonic()
    if args.command == "run":
        result = run(args)
    elif args.command == "check":
        result = correctness(args.backend,args.profile)
    elif args.command == "crash":
        result = crash_check(args.backend)
    else:
        db = connect(args.backend)
        if args.command == "seed":
            db.seed(PROFILES[args.profile])
            result = {"backend":args.backend,"profile":args.profile,"dataset":PROFILES[args.profile].describe(),"seed_seconds":round(time.monotonic()-begin,3)}
        elif args.command == "verify":
            result = db.verify()
        else:
            result = db.metadata()
        db.close()
    result["recorded_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    text = json.dumps(result,indent=2,sort_keys=True)
    if args.output:
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(text+"\n")
    print(text)


if __name__ == "__main__":
    main()

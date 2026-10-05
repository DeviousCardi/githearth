# Local metadata screening results

Status: preliminary single-host evidence; production qualification remains open. The implementation decision is recorded separately in ADR 0001.

All numbers below describe the synthetic application workload, not registered-user capacity or raw database limits. Each row reports the median of repeated runs; p99 is the median of each run's successful-operation p99, not a pooled percentile. Failed requests are counted separately.

## Repeated runs

| Profile | Scenario | Clients | Backend | Runs | Median ops/s | Range ops/s | Median p99 ms | Errors |
| --- | --- | ---: | --- | ---: | ---: | --- | ---: | ---: |
| solo | mixed | 1 | foundationdb | 3 | 379.5 | 266.9–383.1 | 6.882 | 0 |
| solo | mixed | 1 | postgres | 3 | 742.7 | 712.0–744.7 | 3.040 | 0 |
| solo | mixed | 8 | foundationdb | 3 | 1,594.1 | 1,221.1–1,971.0 | 20.134 | 0 |
| solo | mixed | 8 | postgres | 3 | 4,496.4 | 4,485.1–4,554.3 | 5.018 | 0 |
| solo | mixed | 32 | foundationdb | 3 | 3,925.4 | 3,122.7–4,023.8 | 52.875 | 0 |
| solo | mixed | 32 | postgres | 3 | 5,729.5 | 5,426.3–5,745.3 | 50.838 | 0 |
| small | hot | 8 | foundationdb | 3 | 224.4 | 213.1–226.1 | 104.552 | 3 |
| small | hot | 8 | postgres | 3 | 369.3 | 367.1–374.3 | 4.669 | 1 |
| small | mixed | 1 | foundationdb | 3 | 443.3 | 314.9–443.4 | 6.341 | 0 |
| small | mixed | 1 | postgres | 3 | 738.7 | 722.9–745.4 | 3.160 | 0 |
| small | mixed | 8 | foundationdb | 3 | 1,861.8 | 1,149.0–1,865.5 | 16.980 | 0 |
| small | mixed | 8 | postgres | 3 | 4,535.5 | 4,498.3–4,540.7 | 4.809 | 0 |
| small | mixed | 32 | foundationdb | 3 | 3,545.1 | 3,416.7–3,608.4 | 46.446 | 0 |
| small | mixed | 32 | postgres | 3 | 5,639.8 | 5,622.6–5,746.1 | 51.521 | 0 |
| growth | mixed | 1 | foundationdb | 3 | 262.2 | 254.5–369.8 | 9.811 | 0 |
| growth | mixed | 1 | postgres | 3 | 714.0 | 706.1–745.3 | 3.176 | 0 |
| growth | mixed | 8 | foundationdb | 3 | 1,637.9 | 1,170.8–1,892.7 | 16.473 | 0 |
| growth | mixed | 8 | postgres | 3 | 4,589.1 | 4,584.4–4,612.7 | 4.388 | 0 |
| growth | mixed | 32 | foundationdb | 3 | 3,816.7 | 3,186.4–4,296.9 | 22.123 | 0 |
| growth | mixed | 32 | postgres | 3 | 7,244.9 | 7,034.6–7,381.3 | 44.185 | 0 |
| enterprise | hot | 8 | foundationdb | 3 | 183.6 | 183.0–190.7 | 217.300 | 2 |
| enterprise | hot | 8 | postgres | 3 | 377.8 | 366.0–386.3 | 4.621 | 2 |
| enterprise | mixed | 1 | foundationdb | 3 | 352.7 | 351.7–353.4 | 6.680 | 0 |
| enterprise | mixed | 1 | postgres | 3 | 726.1 | 718.6–740.2 | 3.333 | 0 |
| enterprise | mixed | 8 | foundationdb | 3 | 1,644.6 | 1,570.8–1,743.7 | 17.746 | 0 |
| enterprise | mixed | 8 | postgres | 3 | 4,308.5 | 4,097.2–4,319.1 | 5.422 | 0 |
| enterprise | mixed | 32 | foundationdb | 3 | 3,443.6 | 3,211.0–3,450.2 | 49.130 | 0 |
| enterprise | mixed | 32 | postgres | 3 | 5,059.1 | 4,994.6–5,274.2 | 53.873 | 0 |

## Process recovery

The test first commits 100 unique markers, kills the benchmark database container, restarts it with the same volume, and reads every marker back. The host and disk remain intact. Times include restart and marker verification, not just database startup. This is not a power-loss, quorum, or disaster-recovery test.

| Profile | Backend | Acknowledged markers | Lost markers | Restart and verify seconds |
| --- | --- | ---: | ---: | ---: |
| enterprise | foundationdb | 100 | 0 | 1.497 |
| growth | foundationdb | 100 | 0 | 1.483 |
| small | foundationdb | 100 | 0 | 1.492 |
| solo | foundationdb | 100 | 0 | 1.525 |
| enterprise | postgres | 100 | 0 | 3.236 |
| growth | postgres | 100 | 0 | 2.219 |
| small | postgres | 100 | 0 | 1.205 |
| solo | postgres | 100 | 0 | 1.2 |

## Operation failures

- `foundationdb-enterprise/hot-w8-r0.json`: 1 × `FDBError: b'Operation aborted because the transaction timed out' (1031)`; failed-request max 5000.237 ms.
- `foundationdb-enterprise/hot-w8-r2.json`: 1 × `FDBError: b'Operation aborted because the transaction timed out' (1031)`; failed-request max 5000.26 ms.
- `foundationdb-small/hot-w8-r0.json`: 1 × `FDBError: b'Operation aborted because the transaction timed out' (1031)`; failed-request max 5000.272 ms.
- `foundationdb-small/hot-w8-r2.json`: 2 × `FDBError: b'Operation aborted because the transaction timed out' (1031)`; failed-request max 5000.231 ms.
- `postgres-enterprise/hot-w8-r1.json`: 2 × `SerializationFailure: could not serialize access due to concurrent update`; failed-request max 5015.998 ms.
- `postgres-small/hot-w8-r0.json`: 1 × `SerializationFailure: could not serialize access due to concurrent update`; failed-request max 5005.461 ms.

## Resource evidence and limits

Each backend has a 4-CPU quota and 3 GiB memory limit. FoundationDB runs four server processes in one failure domain with single redundancy and the ssd-2 engine; PostgreSQL uses its normal server/backend process model with synchronous local commits. The clients share the host with the database.

Each backend/profile directory contains timestamped `resources.jsonl`, allocated volume size, database configuration, correctness checks, and post-recovery invariant checks. Docker memory observations exclude some reclaimable cache and are not directly comparable to configured cache sizes. Allocated volume sizes include backend-specific logs and allocation behavior; they are not equivalent logical-data sizes.

Warmup seconds: [2.0]; requested measurement seconds: [6.0]; repetitions per comparison row: [3]. The suite rotates concurrency order and reseeds per repetition. These short, closed-loop runs expose gross differences and contention; they do not establish steady-state SLOs. Seed and verification work can affect cache and background cleanup. Reported ranges must be considered before interpreting small differences.

The fixture uses simple, compressible text, basic memberships, and 100 issues per repository. A 100,000-account dataset is not 100,000 active sessions. Complex permission graphs, real repository distributions, lease expiry, outbox draining, multi-node latency, backups, online migrations, and working sets beyond cache are not qualified by this result.

The comparison includes Python driver, serialization, query shape, secondary-index maintenance, and retry-policy costs. A Rust confirmation and equal-durability distributed test are required before attributing the result to a final production architecture.

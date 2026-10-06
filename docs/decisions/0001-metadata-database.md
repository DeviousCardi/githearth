# ADR 0001: PostgreSQL for the first implementation

Date: 2026-10-27.

Status: **provisional implementation decision**. PostgreSQL is the reference metadata backend for the next single-node product milestone. Production qualification, distributed topology, and capacity claims remain pending.

## Decision

Proceed with PostgreSQL for the first Rust metadata adapter and integrated product slice. Keep FoundationDB's experiment and results available for comparison. Do not ship a runtime switch between two metadata engines or build the product around a generic lowest-common-denominator persistence interface.

This selects the next implementation direction. It does not certify the completed forge, a production 100,000-user installation, or PostgreSQL sharding. Reopen the decision if the remaining transaction, recovery, or distributed requirements invalidate it.

## Evidence

The [completed local screening report](../../experiments/metadata/results/local-screening-2026-10-27/REPORT.md) contains 84 measured runs: four dataset profiles, two backends, three concurrency levels, three repetitions, and additional hot-issue runs for the small and enterprise profiles. Both backends had a 4-CPU quota and 3 GiB memory cap. The account range was 1–100,000; the largest fixture contained one million issues in 10,000 repositories within one organization.

For the mixed workload with eight concurrent client processes:

| Dataset | PostgreSQL median ops/s | FoundationDB median ops/s | PostgreSQL median successful p99 | FoundationDB median successful p99 |
| --- | ---: | ---: | ---: | ---: |
| 100 accounts | 4,535.5 | 1,861.8 | 4.809 ms | 16.980 ms |
| 100,000 accounts | 4,308.5 | 1,644.6 | 5.422 ms | 17.746 ms |

PostgreSQL delivered higher median mixed-workload throughput across the tested account profiles and concurrency levels. This was not a universal latency win: FoundationDB had lower median successful p99 in several 32-client comparisons, including the growth profile. Read the full table rather than selecting only favorable numbers.

No mixed-workload operation failed in the completed screening. The hot-issue workload recorded eight failed requests across both engines: three exhausted PostgreSQL's serialization-retry budget and five hit FoundationDB's transaction timeout. Successful-operation percentiles exclude these failed requests; their five-second durations and counts are preserved separately.

All recorded event/counter and claim consistency checks passed. Each of eight process-restart tests retained all 100 previously acknowledged markers. These tests kept the host and data volumes intact. They establish neither power-loss safety nor replicated failure tolerance.

## Why this direction

The primary target is a small self-hosted installation that can grow. PostgreSQL's local measured throughput and eight-client latency were stronger for the current metadata operations. Its relational indexes also express the issue-list and membership workload directly; this is a property of the tested application model, not a claim that SQL is always faster.

PostgreSQL is available under its permissive PostgreSQL License, so this choice does not introduce a required proprietary database service. [PostgreSQL license](https://www.postgresql.org/about/licence/).

The selection is based on the current measurements and product constraints, not on minimizing development time. FoundationDB remains a serious alternative if a demonstrated distributed transaction or horizontal scaling requirement outweighs this local advantage.

## Limits and possible reversals

- The adapters are Python research code. Driver, serialization, query shape, and retry behavior contribute to the measurements. Confirm the key operations in Rust before treating the numbers as production estimates.
- Six-second measurement windows and two-second warmups are short. FoundationDB showed substantial first-run variation in some profiles. Run sustained tests with controlled background work before setting an SLO.
- Both databases share a host with their load generators. Docker CPU quotas and client CPU consumption can influence high-concurrency tails.
- The data is synthetic and compressible. Permission graphs, custom-field queries, integration traffic, outbox consumers, and CI leases are not fully represented.
- Updating one issue counter synchronously is a contention hotspot. Compare separate or sharded counters and explicit retry/fairness policies before adopting this schema for production.
- A large single tenant cannot be scaled indefinitely merely by assigning different tenants to separate PostgreSQL servers. Establish a measured capacity boundary and design finer partitioning if required.
- Distributed PostgreSQL must provide tested synchronous replication, safe promotion, and fencing. Read replicas alone are not horizontal write scaling.
- FoundationDB's clustered scaling behavior was not tested on independent machines. This report does not rank its distributed architecture against PostgreSQL's.

## Next implementation and release gates

1. Implement a Rust PostgreSQL adapter for the agreed commands and run the same contract checks.
2. Replace the research schema with explicit versioned migrations and production identity/tenant ownership rules.
3. Validate backup and restore into a fresh installation and document recovery procedures.
4. Add permissions/revocation, merge intents, leases, and outbox consumption to the benchmark.
5. Measure sustained load, realistic payloads and distributions, and data larger than cache.
6. Qualify a single-region distributed deployment against the failure contract before advertising HA.

Until these gates pass, the first integrated product slice is experimental. No account-count capacity guarantee is published.

## Result provenance

Seven backend/profile groups came from screening round 2. PostgreSQL's enterprise fixture initially failed to load because the normal five-second statement timeout also applied to bulk COPY. Round 3 changed the fixture transaction to a five-minute local timeout, which automatically restores the request timeout on completion, and completed the remaining profile. Measured operation logic and resource limits were unchanged.

The combined result manifest, original manifests, source checksums, raw results, resource samples, seed failure, and the earlier incomplete trial are retained with the report. The earlier trial also exposed a resource-capture parsing issue; its measurements are not used in the comparison table.

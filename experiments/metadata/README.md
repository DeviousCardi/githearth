# Metadata database screening

This experiment compares PostgreSQL and FoundationDB for the [shared metadata contract](../../docs/plan/01-metadata-contract.md). It is executable research, not a production backend or an HA benchmark.

## Environment

Linux x86-64, Docker Compose, Python 3.12, and free loopback ports 15432 and 14500–14503 are required. The lab uses project-specific containers and volumes. Each database is limited to 4 CPU cores worth of CPU time and 3 GiB RAM. Run one database at a time. Both use ordinary disk-backed volumes and durable single-node commits. FoundationDB uses four server processes with one shared failure-domain identity, single redundancy, and 128 MiB page cache per process; this lets it use multiple cores without pretending that four processes are four machines.

PostgreSQL 18.6 and FoundationDB 7.3.77 images are pinned by digest. The FoundationDB C library is copied from the same image as the server. Client packages are pinned in `requirements.txt`. Python provides a common multiprocess evaluation harness; the product workspace is Rust.

Setup installs Python packages into the repository's ignored `.bench/python` directory. It does not install a system database or change the system Python environment.

## Run

From the repository root:

```sh
python3 experiments/metadata/lab.py prepare
python3 experiments/metadata/lab.py start postgres
python3 experiments/metadata/lab.py run seed --backend postgres --profile small
python3 experiments/metadata/lab.py run check --backend postgres --profile small
python3 experiments/metadata/lab.py run run --backend postgres --profile small --workers 8 --seconds 10 --output .bench/results/postgres-small.json
python3 experiments/metadata/lab.py run verify --backend postgres
python3 experiments/metadata/lab.py run crash --backend postgres
python3 experiments/metadata/lab.py start foundationdb
python3 experiments/metadata/lab.py run seed --backend foundationdb --profile small
python3 experiments/metadata/lab.py run check --backend foundationdb --profile small
python3 experiments/metadata/lab.py run run --backend foundationdb --profile small --workers 8 --seconds 10 --output .bench/results/foundationdb-small.json
python3 experiments/metadata/lab.py run verify --backend foundationdb
python3 experiments/metadata/lab.py run crash --backend foundationdb
python3 experiments/metadata/lab.py stop
```

`seed` resets only the experiment database/schema namespace and replaces its synthetic records. Never point this harness at production. `start` stops the other benchmark backend; it does not remove its volume. `crash` kills only the selected benchmark container's main process and verifies 100 previously acknowledged markers after restart. This simulates process failure, not disk destruction or power loss.

Available profiles: `solo`, `small`, `growth`, `enterprise`. Available scenarios: `mixed` and `hot`. Supported worker counts: 1–64. A worker is a synchronous client process with its own connection, so these are concurrent in-flight clients rather than registered users.

## Measurement rules

- Connections are ready before synchronized warmup and measurement windows.
- Latency covers all successful transaction attempts, conflict retries, and client-side backoff for a logical operation.
- Failed operations are counted separately; their retries are not included in the successful-operation retry count.
- Throughput uses completed successful operations divided by elapsed time including final in-flight drain.
- Report empty queue claims separately; these are successful reads but do not represent jobs dispatched.
- Report all per-operation percentiles, run repetitions, seed time, database configuration, Docker resource observations, and errors.
- Use at least three repetitions for comparisons. Rotate worker order. Do not compare during compilation, seeding, correctness scans, or other heavy work.
- The load model is closed loop. Its latency does not include a hypothetical external arrival queue and cannot establish an open-loop saturation SLO.
- Database adapter behavior and client overhead are part of the result. Check client CPU usage before attributing a plateau to the database.

The first results are a local screening round. Production selection also requires the additional workloads and distributed tests listed in the metadata contract.

## Full screening suite

After preparation, run the four profiles with three repetitions:

```sh
python3 experiments/metadata/suite.py --output .bench/results/my-screening --seconds 6 --repetitions 3
python3 experiments/metadata/report.py .bench/results/my-screening
```

The suite rotates concurrency order, reseeds each repetition, captures Docker metrics, verifies invariants, and tests process recovery. It stops the lab when done. Operation timeouts remain in the report; invariant failures stop the suite. The report derives durations and repetition counts from the recorded results.

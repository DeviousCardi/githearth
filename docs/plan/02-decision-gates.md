# Foundation decisions and order of work

Updated 2026-10-27. This records decisions from the project discussion separately from engineering proposals.

## Agreed requirements

- Full forge ambition, with throughput and customer freedom prioritized over speed of delivery.
- One setup experience; databases and other dependencies may run as separate processes.
- Single-node and distributed modes of one product; single-region distributed operation comes first.
- Scale targets range from 1–100 accounts to organizations with approximately 100,000 accounts.
- Test both small and large profiles, with small self-hosted installations prioritized.
- Evaluate PostgreSQL and FoundationDB; consider another database when there is a concrete technical reason.
- AI and development environments remain optional future modules; no hosted service is assumed.
- Be explicit about tolerated failures. An unavailable write is different from a lost acknowledged write.

## Gate A: workload contract

Deliver a domain map, tenant ownership rules, atomicity requirements, idempotency semantics, reference-update boundaries, and concrete dataset profiles. Separate account count from request concurrency. Capture expensive queries and contention instead of benchmarking only raw inserts.

The first version is in [metadata contract](01-metadata-contract.md). Review new product features against this contract before treating database results as representative of them.

## Gate B: local database screening

Implement equivalent application operations using PostgreSQL transactions/indexes and FoundationDB transactions/explicit indexes. Compare repeated small and large fixtures, bounded concurrency, hot-key contention, durable commits, resource use, and process restart recovery.

Correctness checks are a prerequisite for ranking performance. Publish the exact source, versions, resource limits, workload, durations, errors, and raw results. A local screening result can favor a candidate; it does not certify 100,000-user production capacity.

Additional candidates must have a permissive or otherwise acceptable open-source license, a maintainable Rust access path, transaction guarantees suitable for the workload, and a specific benefit. A distributed SQL candidate is worth a later spike if PostgreSQL's sharding model is unacceptable and FoundationDB's query/index implementation cost is substantial. No third engine is added to this first round solely to enlarge the comparison.

## Gate C: confirm the production metadata engine

Local screening has provisionally selected PostgreSQL for the next implementation milestone; see [ADR 0001](../decisions/0001-metadata-database.md). The checks below qualify the production commitment and may reopen that decision.

Before a durable product commitment, resolve the remaining risks that could reverse the local result:

1. Repeat important operations using production-style Rust adapters to measure driver and application overhead.
2. Add nested permissions and revocation, richer filtered lists, merge-intent transitions, job leases, and outbox consumption.
3. Validate backup and restore on a fresh deployment, not just restart in place.
4. Test three-node durability and failover under comparable settings. Local process topology tests are useful, but independent machines are required for the availability claim.
5. For PostgreSQL, describe HA ownership/fencing and the boundary at which tenant sharding is required. For FoundationDB, describe index ownership, online migrations, pagination, and observability.
6. Measure sustained load, working sets beyond cache, growth/cleanup, and an open-loop arrival model before setting capacity targets.

Selection criteria, in order: required correctness and customer freedom; performance under the chosen durability model; resource efficiency at small scale; credible scale-out behavior; recovery and maintainability. Do not hide a correctness failure inside a weighted score.

The outcome is an architecture decision containing the chosen engine, measured advantages, known disadvantages, operating limits, and conditions that would reopen the decision. Support one primary engine in the product. Experimental adapters do not imply a permanent two-database compatibility layer.

## Gate D: independent Git foundation

The Rust workspace may progress alongside database screening. The first prototype establishes stable repository/object identifiers, a local read contract, bare-repository creation, reference listing, and bounded blob reads through native Git. Test against a real local Git push and malformed identifiers.

Next add process deadlines/output limits, explicit configuration, structured logging, repository lifecycle metadata, object-format detection, and API contracts. Choose a journal/ref-transaction design before exposing a product push endpoint. Native receive-pack must not acknowledge a distributed write before GitHearth's durability requirement is met.

The current reader is operator-only and assumes the operator owns the storage directory. It is not an adversarial multi-user filesystem sandbox. A production storage daemon needs a more complete process and filesystem trust model.

## Gate E: first integrated product slice

After the provisional metadata decision, implement an experimental slice with repository lifecycle, identity and permissions, read APIs, code browsing, and a push transaction integration spike. Include migrations, backups, audit events, and operational diagnostics. Only then add issues/reviews and the merge coordinator on the selected transaction model. Production release remains gated on the qualification work in Gate C.

The first distributed slice adds replica membership, data catch-up, committed reference ordering, fencing, routing, and tested node failure. Expand CI, planning, delivery, enterprise capabilities, security, and ecosystem features using these established contracts.

## Decisions still proposed

| Decision | Current position | When it must be resolved |
| --- | --- | --- |
| Product license | `MIT OR Apache-2.0` recommended; no license files adopted yet | Before external distribution/contributions |
| Metadata engine | PostgreSQL provisionally selected for the first implementation; production qualification open | Before a production release or HA claim |
| Git consensus/journal integration | No implementation selected | Before a replicated push service |
| Exact revocation semantics | New admission must honor revocation; in-flight behavior needs a bounded contract | Before protected writes |
| CI compatibility | GitHub Actions compatibility first is a proposal | Before workflow engine implementation |
| Runner boundary | MicroVMs for untrusted Linux workloads proposed | Before untrusted execution |
| Frontend framework | Server rendering with focused interactive components proposed | Before product UI implementation |
| Multi-region/federation | Deferred; separate requirements | After single-region qualification |

These pending decisions do not block the current workload, database, and local Git experiments.

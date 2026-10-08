# Optional service telemetry

OpenTelemetry traces, metrics and structured failure events, sent via OTLP
HTTP/protobuf to New Relic. Disabled by default; no exporter threads, budget
file or network activity exist until explicitly enabled. Initialization failure
disables monitoring without preventing service startup.

Configuration:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OBSERVABILITY_ENABLED` | `false` | Explicit opt-in |
| `NEW_RELIC_LICENSE_KEY` | unset | Ingest-only license key, never a user key |
| `NEW_RELIC_REGION` | `US` | `US` or `EU`; destination is fixed |
| `OBSERVABILITY_BUDGET_PATH` | service data directory | Absolute persistent ledger path |
| `OBSERVABILITY_DAILY_BYTES` | `10485760` | Attempted serialized payload bytes per UTC day |
| `OBSERVABILITY_TRACE_SAMPLE_RATE` | `0.1` | Successful trace sampling, range 0 to 1 |
| `OBSERVABILITY_EXPORT_INTERVAL_SECONDS` | `30` | Metrics interval, range 10 to 300 |

The ledger survives process/container restarts. Missing, corrupt or locked
ledger state fails closed for export. Failed requests consume the byte budget;
clock rollback cannot reset it. Daily allowance is capped at 50 MiB. This is
a transport-volume guard, not a guarantee about vendor billing: keep the
account on its free plan and account for other applications using that account.

Exporters run on SDK background threads with bounded queues, two-second
network deadlines, no redirects, no proxies and no retries. Playback handlers
never perform collector HTTP or ledger I/O. Successful spans are sampled by
trace ID; failures are retained even with an unsampled incoming parent, subject
to queue and byte limits. Metrics include every measured operation, duration,
in-flight operations, process CPU/RSS and exporter counters. Cancellation is
recorded when an operation or streaming body is dropped. Media frames and
trailers pass through without buffering. Shutdown has a bounded flush window.

Only closed operation names, methods, failure categories, numeric statuses,
durations, media modes and byte counts are exported. No source URLs, titles,
IDs, credentials, request/response bodies, arbitrary log messages, baggage,
tracestate or raw exception strings are forwarded. Local diagnostic logging
remains separate. `OTEL_*` resource/header/endpoint settings cannot expand the
export schema or redirect credentials.

`traceparent()` and `in_context()` explicitly propagate request context across
HTTP calls and spawned futures. Consumers vendor immutable copies of this
crate rather than keeping cross-repository build dependencies.

Verification: `cargo test -p service-telemetry` exercises actual protobuf
export, privacy, trace correlation, sampling, persistent budgets, streaming
completion/cancellation and slow/unavailable collectors with full queues.

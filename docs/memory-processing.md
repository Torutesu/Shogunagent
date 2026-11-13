# Observable memory processing

This milestone connects the multi-principal server's existing ingest, hourly review and loop
consolidation pipeline to an opt-in background worker. It adds a read-only status for each saved
event. It does not change the frozen core tool schemas or turn on model calls by default.

## Enable the worker

Set `SHOGUN_PROCESSING_INTERVAL_SECS=60` in the **sidecar** environment and run the existing
`shogun-api --multi <principal-root>` command with its normal key provider and authentication.
Absent or `0` disables the worker. Accepted enabled intervals are 30–3600 seconds; other values
fail startup. The first pass starts when the server binds, and the interval is a delay after each
pass completes, not a promise that each event becomes ready within that interval.

Each principal needs a relay credential in its encrypted token store. Use the existing
`--store-relay-credential` provisioning flow described in [deployment](../deploy/README.md).
Automatic processing never substitutes local rules for a missing, unreadable or removed relay
credential. Enabling it permits the existing metered relay calls and connector queue draining.
No credential values belong in documentation, logs or version control.

For Helm, use the existing `sidecar.extraEnv` setting:

```yaml
sidecar:
  extraEnv:
    - name: SHOGUN_PROCESSING_INTERVAL_SECS
      value: "60"
```

For Compose, set the variable in the deployment's environment file; it defaults to `0`.
Disable the interval and restart the sidecar to stop automatic scheduling. Persisted inputs,
retry deadlines and receipts survive a restart. An explicitly invoked `--dream-tick` can still
use local rules when no relay credential exists, but cannot consume an unfinished model review
through that fallback.

## Read an event's status

Use an event ID returned by `memory.ingest`, with the same principal and agent identity:

| Interface | Request |
|---|---|
| REST | `GET /v1/memory/processing/42` |
| MCP | `memory.processing_status` with `{"id":42}` |
| CLI | `shogun processing status 42` |

The MCP tool belongs to the `state` group. Clients configured to list only `core` tools can
select `core,state` or `full` to discover it.

MCP also accepts `include_scratch`; REST accepts `?include_scratch=true`. This only includes
scratch the caller is permitted to see. Inaccessible, withheld and absent events return the
same `not_found` error. The endpoint is a free read and returns no source content.

| State | Meaning |
|---|---|
| `pending` | Waiting for review, a running review, or unread consolidation inputs. |
| `ready` | A successful model review has a durable receipt and its emitted consolidation inputs have been consumed. |
| `failed` | Review, consolidation or worker configuration failed; `stage` and a fixed `reason` describe the failure. Work remains retryable. |
| `source_only` | The event is excluded from model review, or was processed only by local rules. |
| `unknown` | An older completed review covers the event, but predates per-event receipts. No success is inferred. |

`ready` is pipeline completion, **not** proof that the model extracted every fact or created a
state row. A valid empty answer can finish successfully. Source-backed recall and state-quality
evaluations remain necessary to measure usefulness.

The response includes `reviewed_at_ms`, `review_age_ms`, `checked_at_ms`, and
`retry_not_before_ms` for a failed review. `worker` is null until an automatic pass has recorded a
heartbeat; otherwise it includes the last tick, configured delay, fixed review outcome, and
`heartbeat_fresh`. A heartbeat older than twice the delay is stale. A long pass can make it stale;
it is a diagnostic signal, not a service-level guarantee. A review outcome does not certify
consolidation; the event's state and stage describe that separately. Status reads never trigger processing.

## Scheduling, recovery and limits

- Reviews operate on closed hourly windows using the existing local calendar. A newly ingested
  event may wait for the hour to close. The first review uses the existing 24-hour lookback;
  older initial history reports `source_only` with `outside_initial_review_window`. Once a
  cycle exists, late imports use the existing arrival-sequence ledger.
- Review failures retain the existing 5, 10, 20, 40, then 60 minute backoff across restarts. A new
  hour may proceed while an older pre-submission failure waits; the older work remains durable.
- Consolidation retains the existing one-hour minimum gap and eight-calls-per-local-day cap.
  Transport errors, malformed output and storage errors preserve unread work for a later call.
- State effects and acknowledgement of consolidation inputs commit atomically. Invalid individual
  claims still fail the existing grounding, confidence and provenance checks.
- Each pass is sequential. An OS file lock on `<principal-root>/.processing.lock` excludes another
  automatic worker or `--dream-tick` using that root. Do not remove this file while a worker runs.
  Process exit releases the lock; durable review leases remain authoritative. The lock is for a
  local shared filesystem, not distributed leader election. Use a single owning sidecar per root.
- Receipts and failure metadata are stored in the same SQLCipher principal database as the source.
  They contain IDs, timestamps, lane names and fixed codes, never bodies. Replacing an event
  invalidates its metadata; deleting it cascades deletion. Deleting all memory also clears the
  worker heartbeat. Logs contain aggregate tick counts.

## Verification and pilot gate

Regression tests cover private-event visibility, receipt invalidation, missing credentials,
exclusive scheduling, durable retry backoff, malformed consolidation followed by restart/retry,
atomic storage failure, source-backed state-quality scenarios, and REST/MCP/CLI response parity.
The scripted lanes exercise real storage and processing interfaces; they do not establish live
provider accuracy, deployed reliability, throughput or latency.

Before a production pilot, run the same flow against a staging relay with approved credentials,
observe pending → ready and source-backed recall from a second agent, restart during processing,
exercise a provider outage, and measure completion latency and recovery. Record that evidence
separately. Neither the deployment nor those live-provider checks is part of this local change.

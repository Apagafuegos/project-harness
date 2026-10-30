# 0016: HTTP transport and the providers crate

Status: Implemented for Milestone 4, step 02. No native provider API adapter.

Date: 2026-10-29

## Boundary and dependencies

`crates/providers` (`harness-providers`) implements shared HTTP, SSE and
credential handling behind core's existing `ModelProvider` seam. Core and CLI
are unchanged. `cargo tree -p harness-core` is byte-identical before and after.
Workspace lints apply, including forbidden unsafe and documented public APIs.

The HTTP stack is reqwest 0.12.28 with defaults disabled, `rustls-tls`, `stream`
and `json` (the latter for POST serialization). No native TLS, OpenSSL or cookie
feature is enabled. Tokio is the runtime; futures-util and async-stream provide
owned streams, base64 decodes JWT payloads, httpdate parses Retry-After dates,
and existing rustix provides safe descriptor opens and UID checks. `tempfile`
is a test dependency only. Added dependencies are declared in the workspace
and consumed only by providers. Test-only Tokio networking/I/O features do not
add a dependency to core. The lockfile grows from **157 to 232 package entries**:
**75 additions, 74 external plus harness-providers**, no removals/version changes
of existing entries. This includes optional and foreign-target entries, not
just the active Linux dependency tree.

## HTTP policy

`TransportConfig` provides connect and first-byte timeouts, explicit loopback
HTTP opt-in and an optional explicitly configured proxy. The first-byte budget
covers connect, headers and the first body byte (or clean empty-body EOF).
Per-inference cancellation/deadline also covers setup, reading error bodies,
stream reads and waiting for queue capacity.

There are **no retries** (`retry::never`), **no redirects**, and **no ambient
system proxies** (`no_proxy`, followed only by an explicitly configured proxy).
Ignoring proxy environment variables prevents credentials and inference content
from being silently routed to an unrelated process by ambient shell/CI settings;
explicit proxy configuration makes that trust decision visible. Proxy Debug is
redacted. A harness/version User-Agent identifies the client.

URLs must use HTTPS, with no embedded username/password or fragment. Only
literal `127.0.0.1` and `[::1]` HTTP are permitted when explicitly enabled for
fixtures. Hostnames such as `localhost` and other 127/8 addresses do not qualify.
HTTP/1 only and no idle connection pooling make response release observable as
TCP close, rather than retaining a pooled idle connection. HTTP/2 optimization
is deferred. Reqwest diagnostics/URLs are never forwarded to persisted errors.

## SSE and lifetime

Defaults: **256 KiB per framed event**, **4 MiB total response bytes**,
**1 MiB / 256 queued events**. The per-event accounting includes fields and
comments, with CRLF treated as one logical line ending; the total-response
counter includes every wire byte. This is stricter than a data-only bound.
Parser output supports event/data/id/retry, multiple data lines, comments,
unknown fields, optional BOM, UTF-8 chunk splits and LF/CRLF/CR endings. IDs and
retry are per-frame metadata; there is no reconnection or Last-Event-ID replay.
An incomplete trailing frame/line is a malformed truncated stream, not success.
Invalid UTF-8 fails rather than being replaced. Adapters must additionally
require their own terminal event; clean SSE EOF alone does not mean completion.

One lazily started reader task owns setup and the response. Its drop guard
aborts that task, never detaches it. Cancellation and deadlines select around
all work, including backpressure, so a paused consumer cannot retain an open
connection. Reader failure drops the response before delivering its safe error.

Queue slots and byte permits are reserved **before parsing the next event**.
Each event conservatively reserves `max_event_bytes`; default byte capacity
therefore admits at most four event reservations, below the 256 count ceiling.
This trades utilization for a simple auditable hard bound. No essential event
is dropped: the reader waits cancellably for capacity. Parsing retains only
one bounded event and the current HTTP chunk outside the delivery queue.
No unbounded read-ahead or producer is created. Invalid configurations (zero
capacity or an event larger than the byte queue) fail before opening HTTP.
Per-event/response overflow is `limit_exceeded` and releases the connection.
Queue capacity cannot be exceeded: it backpressures rather than allocating.

## One safe error mapping

| Outcome | ModelError code |
| --- | --- |
| Connect, DNS, TLS, reset, first-byte timeout | transport |
| 401, 403 | authentication |
| 400, 404, 422 | invalid_request |
| 413, context-length error body | context_overflow |
| 429 | rate_limit |
| 500, 502, 503, 504, overloaded body, other unsuccessful HTTP | provider_failure |
| Invalid UTF-8, framing, truncation, wrong content type | malformed_stream |
| Event/response byte bound | limit_exceeded |

Authentication and 429 status take precedence over body hints. HTTP errors
carry safe `http_<status>` provider codes. Other errors have no provider code.
Retry guidance is parsed without executing a retry: `retry-after-ms` takes
precedence, then Retry-After integer seconds or HTTP date (past dates become
zero, overflow saturates). Provider bodies are read to at most 64 KiB and exposed
only through `TransportError::body` for adapter refinement. Debug/Display and
`model_error()` never include them, credentials, unrestricted headers or URLs.
Adapters must preserve this rule when refining an error.

Cancellation/deadline are distinguished by `is_stopped()`, allowing adapters to
emit `ProviderOutcome::Cancelled` rather than inventing a core error enum.
Their safe fallback ModelError is transport. Core independently owns its
inference deadline and durable outcome, as ADR 0014 specifies.

## Credentials (M4 decision 13)

`CredentialSource` is synchronous: these are small, bounded local/environment
reads, not network authentication operations. An async application may put the
Codex read on its blocking worker. There is deliberately no login, refresh,
write or network method on the trait. `Credential` holds a bearer secret,
extra headers and optional expiry; Debug and Display of it and both sources
are exactly `[redacted]`. Header values are marked sensitive before submission.

- `EnvApiKey::new(name)` captures one environment value at construction. Empty,
  missing or non-Unicode values fail authentication; diagnostics name only the
  validated variable identifier. Both API providers can use this source.
- `CodexLogin` locates `~/.codex/auth.json` or accepts an explicit path. Each
  credential call reopens read-only with no-follow/nonblocking/CLOEXEC flags,
  validates the opened regular-file descriptor's owner and permissions and
  reads at most 64 KiB. Any group/world permissions or a different owner are
  refused. Symlinks are not followed. `tokens.access_token` and account ID are
  required; the account header is `chatgpt-account-id`. JWT payload `exp` is
  decoded solely for early expiry reporting, **not signature verification**.
  Malformed, missing or expired login fails authentication with `run codex login`
  guidance. Re-reading picks up the CLI's refresh without modifying it.
- No Anthropic subscription/OAuth credential source. Anthropic consumer tokens
  are restricted to its own applications by its consumer terms. The native
  Anthropic adapter uses an API key and its eventual opt-in live check skips
  when absent. OpenAI subscription checks may use the Codex CLI ChatGPT login;
  its rotating refresh tokens must never be refreshed by this harness.

Secret values never enter logical requests, provider events, journals or
routine diagnostics. Error-body access is deliberately sensitive and must not
be logged. Tests use sentinel secrets and private temporary fixtures only;
no test opens the user's real login file.

## Offline evidence and scope

`crates/providers/src/tests.rs` uses a loopback-only fake HTTP server and a
crate-private `EchoSseProvider`. Tests cover the mapping table, retry guidance,
bounded bodies, redirects, proxy environment isolation in a child process,
URL policy, IPv6 opt-in, setup/stream cancellation, deadlines/drop (also while
paused/backpressured), framing/chunk boundaries, bounds and secret redaction.
Connection-close assertions are bounded by two seconds. Child test output
(stdout, stderr, panic/log output) is searched for the sentinel.

The public core conformance `run_case` runs **11** scenarios over Echo's real
HTTP submission: Text, Refusal, OutputLimit, ProviderError, RateLimit,
SetupFailure, CancelSetup, CancelStreaming, Drop, Deadline and Request.
The observation decodes the actual received JSON tools/options, not the input
ModelRequest, and records live connection release. Deferred to real adapters:
OneTool, InterleavedTools, MalformedToolJson (Echo has no tool vocabulary), Usage
(no usage vocabulary), NativeState (no replay format), UnsupportedOption (no
native option/capability validation). We do not claim the complete 17-case
`run` manifest or live-provider compatibility in this step.

### Break-on-purpose evidence

Each mutation compiled, made the named test fail (Cargo exit 101), and was
reverted before the complete passing suite. The restored providers suite has
20 passing tests, including 11 scenarios within `echo_conformance`.

| Temporary mutation | Failing test | Result |
| --- | --- | --- |
| Put the actual Authorization header in the HTTP error message | `http_error_taxonomy_retry_and_redaction` | Detected |
| Detach the reader instead of aborting it on drop | `cancel_deadline_drop_close_connection` | Detected within the two-second close bound |
| Remove the per-event bound | `oversized_event` | Detected |
| Enable redirect following | `no_redirect` | Detected |
| Remove the file permission check | `insecure_credential_file` | Detected |

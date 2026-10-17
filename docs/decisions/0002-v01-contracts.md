# 0002: Six v0.1 contracts and persistence/security constraints

Status: Provisionally accepted architecture, as agreed by the project owner.

Date: 2026-10-16

## Decision

Retain the Rust-only, headless-core/client separation from decision 0001 and
organize the design around six contracts: model provider, tools, execution and
security, session storage, context management, and agent runtime.

The following are architectural constraints for v0.1:

- Native provider APIs: OpenAI first, Anthropic second, behind a streaming,
  capability-aware interface with hybrid common/provider-owned content.
- Multimodal content types from the outset, even while the initial adapter is text-only.
- Registry-based `read`, `write`, `edit`, and `bash`; sequential tool execution,
  including when a response proposes multiple calls.
- Trusted built-in Rust filesystem operations may run directly only within
  explicitly authorized workspace boundaries. They still require authorization.
- Every external executable, including routine-looking commands, runs through
  a Linux Bubblewrap backend. No unrestricted fallback is permitted.
- Subprocess networking is denied by default. Additional capabilities require
  explicit, scoped authorization and actual enforcement.
- One authoritative append-only JSONL journal per session, tree entries, durable
  active-head selection, one writer, and explicit corruption/crash recovery.
- Context is independent of history. Begin with rolling summarization, automatic
  and manual compaction, and an inspection report through the headless engine API.
- Tokio orchestration with explicit states and one active turn per session.

Subagents, MCP integration, plugin runtimes, semantic memory, remote workspaces,
and multi-agent orchestration are outside v0.1. Future extensions use the same
boundaries rather than bypassing the runtime or tool registry.

## Consequences

JSONL versus SQLite, the Linux isolation backend, and initial tool concurrency
are no longer open architecture choices. The new constraints supersede the
corresponding options in the original draft documents.

[RFC-001](../rfcs/001-core-types-state-persistence.md) proposes concrete types,
record framing, state transitions, and conformance cases. Its detailed design
remains reviewable; this decision does not claim that the runtime or its
security/persistence guarantees are already implemented.

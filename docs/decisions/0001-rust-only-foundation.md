# 0001: Rust-only foundation with a headless engine

Status: Accepted

Date: 2026-10-16

## Context

The project is a small, extensible coding-agent harness focused on inspectable
session history and context construction. An earlier Rust-engine/Go-TUI proposal
introduced a second toolchain and an immediate interprocess protocol. The chosen
direction is Rust only unless a concrete requirement justifies another language.

## Decision

- Use a Cargo workspace and Rust 2024.
- Select Tokio for asynchronous work and Ratatui for the terminal client.
- Start with two crates: `harness-core`, a headless library, and `harness`, the
  command-line executable and eventual TUI.
- Run the engine and client in one process initially. The executable will own
  the Tokio runtime; library code will not create nested runtimes.
- Separate them with typed application commands and events. Rendering, terminal
  state, and keyboard input belong to the client. Engine state and side effects
  belong to the core.
- Defer JSON-RPC, a separate server binary, and remote connections until a client
  or deployment requirement calls for a transport.
- Keep logical subsystems inside the core until their interfaces warrant
  separate crates. Prefer a few proven boundaries to many placeholder crates.
- Keep immutable session history separate from computed, inspectable context.
- Define the model, tool, and session contracts before substantial runtime work.

## Consequences

One language and build system simplify development and distribution. The engine
can be tested without a terminal. A later transport adapter will have to specify
serialization, protocol versions, request correlation, flow control, and
connection failure semantics; Rust enums alone are not a wire protocol.

The language choice does not establish a security boundary. Generated programs
need separate authorization policy and operating-system enforcement.

Tokio and Ratatui are selected technologies, not features implemented by this
scaffold. Their dependencies will arrive with the first code that uses them.

## Revisit when

A headless server, IDE integration, web client, or remote engine creates a real
need for a process boundary. Another language or a large crate split should have
its own decision record with a concrete benefit and its maintenance cost.

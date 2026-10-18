//! The pure, synchronous agent runtime core.
//!
//! [`Runtime::step`] maps one [`Input`] to a list of [`Effect`]s and does
//! nothing else: no file, network or channel access, no clock, and no new
//! identity except through [`Env`]. A driver performs the effects and feeds
//! their answers back as inputs. Storage remains the authority on lifecycle
//! rules; the core reads its facts from the published snapshot.
//!
//! See `docs/runtime.md` for the turn commits, cancellation rules and the
//! event-sequence resync protocol.
//!
//! A driver executes each `Persist` against storage and answers it. Here a
//! loop starts a turn and runs until the core asks for an inference:
//!
//! ```
//! use std::sync::Arc;
//! use harness_core::{runtime::*, session::*};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let parent = tempfile::tempdir()?;
//! let root = Arc::new(DataRoot::open(parent.path().join("state"), true)?);
//! let configuration = root.put_artifact(br#"{"policy":"example"}"#)?;
//! let model = ModelRef { provider_instance: "mock".into(), model: "scripted".into() };
//! let header = SessionHeader {
//!     workspace: "/workspace".into(),
//!     initial_model: model,
//!     policy_revision: "initial".into(),
//!     configuration,
//! };
//! let mut store = JournalStore::create(root, SessionId::new(), header, Timestamp::now())?;
//! let mut env = SystemEnv;
//! let mut runtime = Runtime::new(store.snapshot(), RuntimeConfig::default(), &mut env);
//!
//! let content = vec![ContentBlock::Text { text: "hello".into() }];
//! let command = Command::StartTurn { command_id: CommandId::new(), content };
//! let mut inputs = vec![Input::Command { request: RequestId(1), command }];
//! let (mut replies, mut inference) = (Vec::new(), None);
//! while let Some(input) = inputs.pop() {
//!     for effect in runtime.step(input, &mut env) {
//!         match effect {
//!             Effect::Persist { token, mut commit, command, at } => {
//!                 if let Some((id, command)) = command {
//!                     store.bind_command(&mut commit, id, &command)?;
//!                 }
//!                 let ack = store.append(commit, at)?;
//!                 inputs.push(Input::Persisted { token, ack, snapshot: store.snapshot() });
//!             }
//!             Effect::StartInference { inference: id, .. } => inference = Some(id),
//!             Effect::Reply { result, .. } => replies.push(result),
//!             _ => {}
//!         }
//!     }
//! }
//! // The reply follows the opening commit's sync; the stream follows the next.
//! assert!(matches!(replies[..], [Ok(Accepted::TurnStarted { .. })]));
//! assert!(inference.is_some());
//! assert_eq!(runtime.phase(), Phase::Durable(RuntimeState::Inferring));
//! # Ok(())
//! # }
//! ```

mod core;
mod driver;
mod env;
mod types;
mod view;
mod worker;

pub use self::core::Runtime;
pub use driver::{DriverConfig, SessionHandle, Subscription, SubscriptionItem};
pub use env::{DeterministicEnv, Env, SystemEnv};
pub use types::*;
pub use view::{ApplyOutcome, ClientView, PartialBlock, PartialResponse, Phase, RuntimeView};
pub use worker::StorageWorker;

#[cfg(test)]
mod tests;

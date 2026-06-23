//! Out-of-band SOP approval plane (EPIC C).
//!
//! The resolution layer on top of EPIC A (the engine singleton) and EPIC B (the
//! durable run store + append-only event log). It provides ONE gate-clearing
//! entry point (`resolve_gate`, added to the engine in a later slice) reachable
//! from four principals - the agent tool, the loopback CLI, the gateway, and the
//! timeout tick - each recorded into B's append-only ledger with a
//! transport-derived principal that a client body can never forge.
//!
//! C0 ships the pure types only (no engine/gateway/CLI wiring): the principal
//! model, the decision/outcome, and the ledger-row mapping onto B's
//! `SopEventRecord`. Subsequent slices add the config mode, the fail-closed
//! timeout, the `resolve_gate` chokepoint, and the out-of-band surfaces.

pub mod decision;
pub mod ledger;
pub mod principal;

pub use decision::{ApprovalDecision, ResolveOutcome};
pub use ledger::{GateEventKind, GateLedgerEntry};
pub use principal::{ApprovalPrincipal, ApprovalSource};

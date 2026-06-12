//! GitHub App channel — converse with the agent through GitHub issue and
//! pull-request comments, authenticated as a GitHub App.
//!
//! Polling-based: inbound events come from the REST API on a `since`
//! cursor (no webhook, no inbound network exposure). Outbound replies are
//! issue comments posted as the app's `<slug>[bot]` identity, with draft
//! streaming via comment edits.
//!
//! Layout (contract-first):
//! - [`types`] — shared constants, newtypes, REST payloads, error enum.
//! - [`auth`] — private key + RS256 JWT + token cache (only key-touching file).
//! - [`api`] — typed REST wrappers; credentials passed in.
//! - [`events`] — pure payload → `ChannelMessage` mapping and filters.
//! - [`poll`] — pure cursor/dedup state machine.
//! - [`channel`] — composition root implementing the `Channel` trait.

mod api;
mod auth;
mod channel;
mod events;
mod poll;
mod types;

pub use channel::GithubChannel;

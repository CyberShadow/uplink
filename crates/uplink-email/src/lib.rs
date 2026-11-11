//! Email: the mail store, mailbox state and its log entries, queries, and
//! the provider sync tasks (SPEC §8).

pub mod account;
pub mod compose;
pub mod gmail;
pub mod m365;
pub mod mailbox;
pub mod oauth;
pub mod outbox;
pub mod parse;
pub mod query;
pub mod remote;
pub mod search;
pub mod service;

#[cfg(test)]
mod tests;

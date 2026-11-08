//! Email: the mail store, mailbox state and its log entries, queries, and
//! the provider sync tasks (SPEC §8).

pub mod mailbox;
pub mod parse;
pub mod query;
pub mod service;
pub mod store;

#[cfg(test)]
mod tests;

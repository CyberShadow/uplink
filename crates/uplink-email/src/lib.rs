//! Email: the mail store, mailbox state and its log entries, queries, and
//! the provider sync tasks (SPEC §8).

pub mod account;
pub mod compose;
pub mod gmail;
pub mod image_senders;
pub mod imap;
pub mod m365;
pub mod mail_server;
pub mod mailbox;
pub mod oauth;
pub mod outbox;
pub mod parse;
pub mod pop3;
pub mod query;
pub mod remote;
pub mod service;

#[cfg(test)]
mod tests;

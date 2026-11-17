//! Email: the mail store, mailbox state and its log entries, queries, and
//! the provider sync tasks (SPEC §8).

pub mod account;
pub mod actions;
pub mod compose;
pub mod gmail;
pub mod identity;
pub mod image_rules;
pub mod imap;
pub mod jmap;
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
pub mod watching;

/// The kinds of the mail accounts, as recorded (`account.kind`): one per
/// provider.
pub const KINDS: [&str; 5] = [
    gmail::sync::KIND,
    m365::sync::KIND,
    imap::sync::KIND,
    pop3::sync::KIND,
    jmap::sync::KIND,
];

#[cfg(test)]
mod tests;

//! What accounts at any mail server, IMAP or POP3, have alike: signing in,
//! sending by SMTP, and threads by headers (SPEC §8.10–8.12, §13.4,
//! §13.7).

pub mod sign_in;
pub mod smtp;

use mail_parser::{HeaderName, MessageParser};

/// The conversation of a message, as its headers tell (SPEC §4.4): the
/// first message its References name, else the one it replies to, else
/// itself. Mail servers give no thread ids.
pub fn thread_of(raw: &[u8]) -> Option<String> {
    let message = MessageParser::new().with_message_ids().parse_headers(raw)?;
    // One id is text, more a list.
    let first = |name| {
        let value = message.header(name)?;
        value
            .as_text_list()
            .and_then(|l| l.first().map(|s| s.to_string()))
            .or_else(|| value.as_text().map(str::to_owned))
    };
    first(HeaderName::References)
        .or_else(|| first(HeaderName::InReplyTo))
        .or_else(|| message.message_id().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_are_the_first_message_referred_to() {
        let thread = |headers: &str| thread_of(format!("{headers}\r\n\r\nbody").as_bytes());
        assert_eq!(
            thread("Message-ID: <c@x>\r\nIn-Reply-To: <b@x>\r\nReferences: <a@x> <b@x>"),
            Some("a@x".into())
        );
        assert_eq!(
            thread("Message-ID: <b@x>\r\nIn-Reply-To: <a@x>"),
            Some("a@x".into())
        );
        assert_eq!(thread("Message-ID: <a@x>"), Some("a@x".into()));
        assert_eq!(thread("Subject: none"), None);
    }
}

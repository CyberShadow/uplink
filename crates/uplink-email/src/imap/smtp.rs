//! Sending an account's mail by SMTP submission (SPEC §8.11).

use super::session::{Secret, SignIn};
use crate::account::SendError;
use anyhow::anyhow;
use mail_parser::{Address, HeaderName, MessageParser};
use mail_send::{
    smtp::message::Parameters, Credentials, Error as SmtpError, SmtpClient, SmtpClientBuilder,
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;
use uplink_config::{ImapAccountConfig, Security};

/// How long the server may take to answer.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Submits `raw` (RFC 5322, as built) from the account to whom its To,
/// Cc and Bcc name; Bcc is not in what they receive. Which step failed
/// says whether the message may have gone out: only once all of it is
/// written, and before the server's answer, may it have.
pub async fn send(
    account: &ImapAccountConfig,
    sign_in: &SignIn,
    tls: &TlsConnector,
    raw: &[u8],
) -> Result<(), SendError> {
    let recipients = recipients(raw);
    if recipients.is_empty() {
        return Err(SendError::Rejected("the message has no recipients".into()));
    }
    let user = account.user().to_owned();
    let credentials = match sign_in.secret().await.map_err(SendError::NotSent)? {
        Secret::Password(password) => Credentials::new(user, password),
        Secret::AccessToken(token) => Credentials::new_xoauth2(user, token),
    };
    let host = &account.smtp.host;
    let connected =
        SmtpClientBuilder::new_with_tls_connector(host.clone(), account.smtp_port(), tls.clone())
            .implicit_tls(account.smtp.security == Security::Tls)
            .credentials(credentials)
            .timeout(TIMEOUT)
            .connect()
            .await;
    let mut client = match connected {
        Ok(client) => client,
        Err(e) => {
            if matches!(e, SmtpError::AuthenticationFailed(_)) {
                sign_in.refused().await;
            }
            return Err(SendError::NotSent(
                anyhow!("{e}").context(format!("connecting to {host}")),
            ));
        }
    };
    let result = submit(
        &mut client,
        &account.address,
        &recipients,
        &without_bcc(raw),
    )
    .await;
    // Whatever the server says to leaving, the message is settled.
    let _ = client.quit().await;
    result
}

async fn submit<T: AsyncRead + AsyncWrite + Unpin>(
    client: &mut SmtpClient<T>,
    from: &str,
    recipients: &[String],
    message: &[u8],
) -> Result<(), SendError> {
    let params = Parameters::default();
    client
        .mail_from(from, &params)
        .await
        .map_err(|e| before_data(e, "MAIL FROM"))?;
    for recipient in recipients {
        client
            .rcpt_to(recipient, &params)
            .await
            .map_err(|e| before_data(e, &format!("RCPT TO {recipient}")))?;
    }
    client.data(message).await.map(drop).map_err(|e| match e {
        // The server answered: it did not take the message.
        SmtpError::UnexpectedReply(reply) => refusal(reply.code, &reply.message),
        // Written, perhaps whole, but unanswered.
        e => SendError::Unknown(anyhow!("{e}").context("submitting the message")),
    })
}

/// A failure before the message was written: it did not go out.
fn before_data(e: SmtpError, step: &str) -> SendError {
    match e {
        SmtpError::UnexpectedReply(reply) => refusal(reply.code, &reply.message),
        e => SendError::NotSent(anyhow!("{e}").context(step.to_owned())),
    }
}

/// The server's answer refusing the message: for good (5xx), or for now.
fn refusal(code: u16, message: &str) -> SendError {
    let reason = format!("{code} {message}");
    if code >= 500 {
        SendError::Rejected(reason)
    } else {
        SendError::NotSent(anyhow!(reason))
    }
}

/// The addresses the message's To, Cc and Bcc name, each once.
fn recipients(raw: &[u8]) -> Vec<String> {
    let Some(message) = MessageParser::new()
        .with_address_headers()
        .parse_headers(raw)
    else {
        return vec![];
    };
    let mut seen = std::collections::HashSet::new();
    // Every header of each, not only the last.
    [HeaderName::To, HeaderName::Cc, HeaderName::Bcc]
        .into_iter()
        .flat_map(|name| message.header_values(name))
        .filter_map(|value| value.as_address())
        .flat_map(Address::iter)
        .filter_map(|a| a.address().map(str::to_owned))
        .filter(|a| seen.insert(a.to_lowercase()))
        .collect()
}

/// `raw` without its Bcc header, as its recipients receive it.
fn without_bcc(raw: &[u8]) -> Vec<u8> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(raw.len(), |i| i + 2);
    let (headers, body) = raw.split_at(end);
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in headers.split_inclusive(|&b| b == b'\n') {
        let continuation = line.first().is_some_and(|b| *b == b' ' || *b == b'\t');
        if !continuation {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcc_is_left_out() {
        let raw = b"From: a@x\r\nBcc: b@x,\r\n c@x\r\nTo: d@x\r\nBCC: e@x\r\nSubject: s\r\n\r\nBcc: body\r\n";
        assert_eq!(
            without_bcc(raw),
            b"From: a@x\r\nTo: d@x\r\nSubject: s\r\n\r\nBcc: body\r\n"
        );
        assert_eq!(recipients(raw), ["d@x", "b@x", "c@x", "e@x"]);
    }

    #[test]
    fn a_message_without_a_body_keeps_its_headers() {
        assert_eq!(without_bcc(b"To: d@x\r\nBcc: b@x\r\n"), b"To: d@x\r\n");
    }
}

//! A POP3 client (RFC 1939), of what uplink uses: signing in (RFC 2595's
//! STLS, RFC 5034's AUTH), listing by UIDL, retrieving and deleting.

use crate::mail_server::sign_in::{self, Secret, SignIn};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use tokio_rustls::{client::TlsStream, rustls::pki_types::ServerName, TlsConnector};
use uplink_config::{Pop3AccountConfig, Security};

/// How long the server may take to answer.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The server refused a command (`-ERR`), for this reason.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// Commands and their answers, over `T`.
struct Lines<T> {
    io: BufReader<T>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Lines<T> {
    /// A line, without its CRLF.
    async fn line(&mut self) -> Result<Vec<u8>> {
        let mut line = vec![];
        tokio::time::timeout(TIMEOUT, self.io.read_until(b'\n', &mut line))
            .await
            .context("the server did not answer in time")??;
        ensure!(
            line.ends_with(b"\r\n"),
            "the server ended the connection mid-line"
        );
        line.truncate(line.len() - 2);
        Ok(line)
    }

    async fn send(&mut self, command: &str) -> Result<()> {
        ensure!(
            !command.contains(['\r', '\n']),
            "a POP3 command cannot hold a line break"
        );
        let io = self.io.get_mut();
        io.write_all(format!("{command}\r\n").as_bytes()).await?;
        io.flush().await?;
        Ok(())
    }

    /// See [`status`].
    async fn status(&mut self) -> Result<String> {
        status(&self.line().await?)
    }

    async fn command(&mut self, command: &str) -> Result<String> {
        self.send(command).await?;
        self.status().await
    }

    /// The lines of a multi-line answer, after its status, unstuffed.
    async fn multiline(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut lines = vec![];
        loop {
            let mut line = self.line().await?;
            if line == b"." {
                return Ok(lines);
            }
            if line.starts_with(b".") {
                line.remove(0);
            }
            lines.push(line);
        }
    }
}

/// The text of an answer `+OK`; one `-ERR` is [`Refused`].
fn status(line: &[u8]) -> Result<String> {
    let line = String::from_utf8_lossy(line);
    if let Some(text) = line.strip_prefix("+OK") {
        return Ok(text.trim_start().to_owned());
    }
    if let Some(reason) = line.strip_prefix("-ERR") {
        return Err(Refused(reason.trim_start().to_owned()).into());
    }
    bail!("the server answered {line:?}")
}

/// A POP3 session, signed in: the maildrop is the session's until it
/// quits (RFC 1939 §8), so messages do not come or go meanwhile.
pub struct Client {
    lines: Lines<TlsStream<TcpStream>>,
}

/// Connects to `account`'s POP3 server and signs in.
pub async fn connect(
    account: &Pop3AccountConfig,
    sign_in: &SignIn,
    tls: &TlsConnector,
) -> Result<Client> {
    let host = &account.pop3.host;
    let port = account.pop3_port();
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let name = ServerName::try_from(host.clone())?;
    let mut lines = match account.pop3.security {
        Security::Tls => {
            let mut lines = Lines {
                io: BufReader::new(tls.connect(name, tcp).await?),
            };
            lines.status().await.context("the server's greeting")?;
            lines
        }
        Security::Starttls => {
            let mut plain = Lines {
                io: BufReader::new(tcp),
            };
            plain.status().await.context("the server's greeting")?;
            plain.command("STLS").await.context("STLS")?;
            // What came before TLS cannot be trusted to be the server's.
            ensure!(
                plain.io.buffer().is_empty(),
                "the server sent more than it was asked before TLS"
            );
            Lines {
                io: BufReader::new(tls.connect(name, plain.io.into_inner()).await?),
            }
        }
    };
    let user = account.login().user;
    let signed_in = match sign_in.secret().await? {
        Secret::Password(password) => {
            async {
                lines.command(&format!("USER {user}")).await?;
                lines.command(&format!("PASS {password}")).await
            }
            .await
        }
        Secret::AccessToken(token) => {
            let response = STANDARD.encode(sign_in::xoauth2(user, &token));
            lines.send(&format!("AUTH XOAUTH2 {response}")).await?;
            let answer = lines.line().await?;
            if answer.starts_with(b"+ ") || answer == b"+" {
                // A challenge carries the error; an empty response ends it.
                lines.command("").await
            } else {
                status(&answer)
            }
        }
    };
    match signed_in {
        Ok(_) => Ok(Client { lines }),
        Err(e) => match e.downcast::<Refused>() {
            Ok(Refused(reason)) => {
                sign_in.refused().await;
                bail!("{host} refused to sign in {user}: {reason}")
            }
            Err(e) => Err(e.context(format!("signing in to {host}"))),
        },
    }
}

impl Client {
    /// The maildrop's messages: their numbers and unique ids, oldest
    /// first.
    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        self.lines.command("UIDL").await.context("UIDL")?;
        self.lines
            .multiline()
            .await?
            .into_iter()
            .map(|line| {
                let line = String::from_utf8(line).context("a UIDL line not UTF-8")?;
                let (number, uid) = line
                    .split_once(' ')
                    .with_context(|| format!("a UIDL line {line:?}"))?;
                Ok((number.parse()?, uid.to_owned()))
            })
            .collect()
    }

    /// Message `number`, as it is, in CRLF lines.
    pub async fn retr(&mut self, number: u32) -> Result<Vec<u8>> {
        self.lines
            .command(&format!("RETR {number}"))
            .await
            .with_context(|| format!("RETR {number}"))?;
        let mut raw = vec![];
        for line in self.lines.multiline().await? {
            raw.extend_from_slice(&line);
            raw.extend_from_slice(b"\r\n");
        }
        Ok(raw)
    }

    /// Marks message `number` deleted: it is, once the session quits.
    pub async fn dele(&mut self, number: u32) -> Result<()> {
        self.lines
            .command(&format!("DELE {number}"))
            .await
            .with_context(|| format!("DELE {number}"))?;
        Ok(())
    }

    /// Ends the session, deleting what was marked.
    pub async fn quit(mut self) -> Result<()> {
        self.lines.command("QUIT").await.context("QUIT")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &'static [u8]) -> Lines<tokio::io::DuplexStream> {
        let (client, mut server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            server.write_all(input).await.unwrap();
            // Kept open, so that reads past the input wait.
            std::future::pending::<()>().await;
        });
        Lines {
            io: BufReader::new(client),
        }
    }

    #[tokio::test]
    async fn multi_line_answers_are_unstuffed() {
        let mut l = lines(b"+OK 2 lines\r\n..dot\r\nplain\r\n.\r\n");
        assert_eq!(l.status().await.unwrap(), "2 lines");
        assert_eq!(
            l.multiline().await.unwrap(),
            [b".dot".to_vec(), b"plain".to_vec()]
        );
    }

    #[tokio::test]
    async fn refusals_carry_their_reason() {
        let mut l = lines(b"-ERR [AUTH] no such user\r\n");
        let e = l.status().await.unwrap_err();
        assert_eq!(e.downcast::<Refused>().unwrap().0, "[AUTH] no such user");
    }

    #[tokio::test]
    async fn lines_must_end_in_crlf() {
        let (client, mut server) = tokio::io::duplex(64);
        server.write_all(b"+OK cut").await.unwrap();
        drop(server);
        let mut l = Lines {
            io: BufReader::new(client),
        };
        assert!(l.status().await.is_err());
    }

    #[tokio::test]
    async fn commands_hold_no_line_breaks() {
        let mut l = lines(b"");
        assert!(l.send("PASS a\r\nDELE 1").await.is_err());
    }
}

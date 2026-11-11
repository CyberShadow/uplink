//! Mailbox names as IMAP writes them: modified UTF-7 (RFC 3501 §5.1.3).

use anyhow::{bail, ensure, Context, Result};
use base64::{alphabet, engine, Engine};

/// Modified base64: `,` for `/`, no padding.
const MODIFIED_BASE64: engine::GeneralPurpose = engine::GeneralPurpose::new(
    &match alphabet::Alphabet::new(
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,",
    ) {
        Ok(alphabet) => alphabet,
        Err(_) => panic!("the modified base64 alphabet is valid"),
    },
    engine::GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(engine::DecodePaddingMode::RequireNone),
);

/// The text of a mailbox name.
pub fn decode(name: &str) -> Result<String> {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let shifted = &rest[amp + 1..];
        let end = shifted
            .find('-')
            .with_context(|| format!("unterminated shift in mailbox name {name:?}"))?;
        if end == 0 {
            out.push('&');
        } else {
            let bytes = MODIFIED_BASE64
                .decode(&shifted[..end])
                .with_context(|| format!("mailbox name {name:?}"))?;
            ensure!(bytes.len() % 2 == 0, "odd UTF-16 in mailbox name {name:?}");
            let units = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
            for c in char::decode_utf16(units) {
                match c {
                    Ok(c) => out.push(c),
                    Err(_) => bail!("unpaired surrogate in mailbox name {name:?}"),
                }
            }
        }
        rest = &shifted[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_decode() {
        for (encoded, text) in [
            ("INBOX", "INBOX"),
            // RFC 3501 §5.1.3's example.
            ("~peter/mail/&U,BTFw-/&ZeVnLIqe-", "~peter/mail/台北/日本語"),
            ("Tom &- Jerry", "Tom & Jerry"),
            ("Entw&APw-rfe", "Entwürfe"),
            ("&2D3eAA-", "😀"),
        ] {
            assert_eq!(decode(encoded).unwrap(), text, "{encoded}");
        }
    }

    #[test]
    fn malformed_names_are_refused() {
        for name in ["&U,BTFw", "&U,B-", "&2D0-"] {
            assert!(decode(name).is_err(), "{name}");
        }
    }
}

//! A bridged room's network, by the bridge's state (MSC2346: `m.bridge`,
//! or `uk.half-shot.bridge` as bridges still send it): its protocol's
//! name and icon, which a room's avatar shows as a badge, as
//! SchildiChat's room list does.

use matrix_sdk::ruma::MxcUri;
use serde_json::Value;
use uplink_protocol::matrix::Bridge;

/// The state event types a bridge tells of itself by, in the order they
/// are looked at.
pub const EVENT_TYPES: [&str; 2] = ["m.bridge", "uk.half-shot.bridge"];

/// The bridge an event's `content` tells of, if its protocol has an id
/// and a valid `mxc://` icon: a bridge without an icon has no badge.
pub fn parse(content: &Value) -> Option<Bridge> {
    let protocol = &content["protocol"];
    let id = protocol["id"].as_str()?;
    let icon = protocol["avatar_url"]
        .as_str()
        .filter(|url| <&MxcUri>::from(*url).is_valid())?;
    let name = protocol["displayname"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(id);
    Some(Bridge {
        name: name.to_owned(),
        icon: icon.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bridge_is_its_protocols_name_and_icon() {
        // As mautrix-signal sends it.
        let content = json!({
            "bridgebot": "@signalbot:example.org",
            "protocol": {
                "id": "signal",
                "displayname": "Signal",
                "avatar_url": "mxc://maunium.net/wPJgTQbZOtpBFmDNkiNEMDUp",
                "external_url": "https://signal.org/",
            },
            "channel": { "id": "a-group" },
        });
        assert_eq!(
            parse(&content),
            Some(Bridge {
                name: "Signal".into(),
                icon: "mxc://maunium.net/wPJgTQbZOtpBFmDNkiNEMDUp".into(),
            })
        );
    }

    #[test]
    fn a_protocol_without_a_name_is_named_by_its_id() {
        let content = json!({
            "protocol": { "id": "slack", "displayname": " ", "avatar_url": "mxc://a/b" },
        });
        assert_eq!(parse(&content).map(|b| b.name), Some("slack".into()));
    }

    #[test]
    fn a_protocol_without_an_id_or_a_valid_icon_is_none() {
        for content in [
            json!({ "protocol": { "id": "irc" } }),
            json!({ "protocol": { "id": "irc", "avatar_url": "https://example.org/irc.png" } }),
            json!({ "protocol": { "avatar_url": "mxc://a/b" } }),
            json!({}),
        ] {
            assert_eq!(parse(&content), None, "{content}");
        }
    }
}

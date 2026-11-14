//! A mail account's identities (SPEC §3.2): the addresses it sends from,
//! which are the user's in its mail.

use uplink_protocol::mail::Identity;

/// The identity `address` is of: the one of that address, else of its
/// domain. Addresses are compared without regard to case.
pub fn of<'a>(identities: &'a [Identity], address: &str) -> Option<&'a Identity> {
    let at = |domain: &str| {
        address
            .rsplit_once('@')
            .is_some_and(|(_, d)| d.eq_ignore_ascii_case(domain))
    };
    identities
        .iter()
        .find(|i| matches!(i, Identity::Address { address: a, .. } if a.eq_ignore_ascii_case(address)))
        .or_else(|| {
            identities
                .iter()
                .find(|i| matches!(i, Identity::Domain { domain, .. } if at(domain)))
        })
}

/// The name mail from the identity is sent under.
pub fn name(identity: &Identity) -> Option<&str> {
    match identity {
        Identity::Address { name, .. } | Identity::Domain { name, .. } => name.as_deref(),
    }
}

/// The account's own address, the identity mail is sent from unless a
/// draft names another: the first.
pub fn default(identities: &[Identity]) -> &str {
    match identities.first() {
        Some(Identity::Address { address, .. }) => address,
        other => unreachable!("a mail account's identities begin with its address: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_of_its_identity_else_of_its_domains() {
        let identities = [
            Identity::Address {
                address: "alice@example.org".into(),
                name: Some("Alice".into()),
            },
            Identity::Domain {
                domain: "example.com".into(),
                name: Some("Shop".into()),
            },
            Identity::Address {
                address: "boss@example.com".into(),
                name: Some("The Boss".into()),
            },
        ];
        let name_of = |address: &str| of(&identities, address).map(name);
        assert_eq!(name_of("Alice@Example.ORG"), Some(Some("Alice")));
        assert_eq!(name_of("anything@EXAMPLE.com"), Some(Some("Shop")));
        assert_eq!(name_of("boss@example.com"), Some(Some("The Boss")));
        assert_eq!(name_of("bob@example.org"), None);
        assert_eq!(name_of("example.com"), None);
        assert_eq!(name_of("x@sub.example.com"), None);
        assert_eq!(default(&identities), "alice@example.org");
    }
}

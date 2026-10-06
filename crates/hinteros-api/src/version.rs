//! API versions (`MAJOR.MINOR`) and handshake negotiation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

impl ApiVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Whether a client built against `client` can talk to a server implementing `self`.
    ///
    /// Majors must match. While the major is 0, every minor is breaking, so the minor must
    /// match too (the same rule Cargo applies to 0.x versions).
    pub fn is_compatible_with(self, client: ApiVersion) -> bool {
        self.major == client.major && (self.major != 0 || self.minor == client.minor)
    }

    /// The version both sides use for this connection: the lower of the two, so neither side
    /// relies on methods, events or fields the other doesn't know. `None` if incompatible.
    pub fn negotiate(self, client: ApiVersion) -> Option<ApiVersion> {
        self.is_compatible_with(client).then(|| self.min(client))
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid API version {0:?}: expected MAJOR.MINOR")]
pub struct ParseVersionError(String);

impl FromStr for ApiVersion {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseVersionError(s.to_owned());
        let (major, minor) = s.split_once('.').ok_or_else(err)?;
        let part = |p: &str| {
            // Same rule as the schema pattern: digits only, no leading zeros.
            if p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
            {
                return Err(err());
            }
            p.parse::<u32>().map_err(|_| err())
        };
        Ok(Self::new(part(major)?, part(minor)?))
    }
}

impl Serialize for ApiVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        assert_eq!("0.1".parse::<ApiVersion>().unwrap(), ApiVersion::new(0, 1));
        assert_eq!(ApiVersion::new(12, 3).to_string(), "12.3");
        for bad in ["", "1", "1.", ".1", "01.0", "1.0.0", "a.b", "-1.0", "+1.0"] {
            assert!(
                bad.parse::<ApiVersion>().is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn negotiation() {
        let v = ApiVersion::new;
        // 0.x: exact match only.
        assert_eq!(v(0, 1).negotiate(v(0, 1)), Some(v(0, 1)));
        assert_eq!(v(0, 2).negotiate(v(0, 1)), None);
        assert_eq!(v(0, 1).negotiate(v(1, 1)), None);
        // 1.x and later: same major, lower minor wins.
        assert_eq!(v(1, 3).negotiate(v(1, 1)), Some(v(1, 1)));
        assert_eq!(v(1, 1).negotiate(v(1, 3)), Some(v(1, 1)));
        assert_eq!(v(2, 0).negotiate(v(1, 9)), None);
    }
}

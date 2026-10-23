use std::{borrow::Cow, fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// How long an object lives, counted from its creation: one or more whole numbers,
/// each followed by `d`, `h`, `m` or `s`, for example `7d`, `48h` or `1d12h`.
///
/// It keeps the text it was written as, so writing an object back does not rewrite
/// `48h` as `2d`. Every string the CRD schema accepts parses; values too large to
/// represent mean "practically forever".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ttl {
    text: String,
    duration: Duration,
}

/// The CRD schema's form of a [`Ttl`]; [`Ttl::from_str`] accepts exactly these.
const PATTERN: &str = "^([0-9]+[dhms])+$";

impl Ttl {
    pub fn duration(&self) -> Duration {
        self.duration
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "invalid TTL {0:?}: expected whole numbers each followed by d, h, m or s, e.g. 7d, 48h or 1d12h"
)]
pub struct ParseTtlError(String);

impl FromStr for Ttl {
    type Err = ParseTtlError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || ParseTtlError(text.to_owned());
        let mut seconds: u64 = 0;
        let mut number: Option<u64> = None;
        for c in text.chars() {
            if let Some(digit) = c.to_digit(10) {
                let n = number.unwrap_or(0);
                number = Some(n.saturating_mul(10).saturating_add(u64::from(digit)));
                continue;
            }
            let unit = match c {
                'd' => 86_400,
                'h' => 3_600,
                'm' => 60,
                's' => 1,
                _ => return Err(error()),
            };
            seconds = seconds.saturating_add(number.take().ok_or_else(error)?.saturating_mul(unit));
        }
        if text.is_empty() || number.is_some() {
            return Err(error());
        }
        Ok(Self {
            text: text.to_owned(),
            duration: Duration::from_secs(seconds),
        })
    }
}

impl fmt::Display for Ttl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl Serialize for Ttl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Ttl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

impl schemars::JsonSchema for Ttl {
    fn schema_name() -> Cow<'static, str> {
        "Ttl".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": PATTERN,
        })
    }
}

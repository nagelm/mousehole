//! RFC 9557 zoned timestamps per docs/rust-rewrite/api-contract.md §2.3.
//! The frontend parses with `Temporal.ZonedDateTime.from`, which THROWS
//! without the bracketed IANA annotation — jiff's `Zoned` Display emits
//! exactly that form (`2026-08-18T09:05:00.123-05:00[America/Chicago]`).

use jiff::{Timestamp, Zoned};

/// Now in the host's IANA zone (TZ env honored by jiff's tzdb lookup).
pub fn now_zoned() -> Zoned {
    Zoned::now()
}

/// Wire format: `Zoned` Display is RFC 9557 with the bracketed annotation.
pub fn to_wire(z: &Zoned) -> String {
    z.to_string()
}

pub fn parse_wire(s: &str) -> Result<Zoned, jiff::Error> {
    s.parse()
}

/// now + fractional seconds, for `nextContactAt` scheduling.
pub fn plus_seconds(z: &Zoned, seconds: f64) -> Zoned {
    let ns = (seconds * 1e9) as i64;
    let ts = z.timestamp() + jiff::SignedDuration::from_nanos(ns);
    zoned_at(ts, z)
}

fn zoned_at(ts: Timestamp, reference: &Zoned) -> Zoned {
    ts.to_zoned(reference.time_zone().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_has_bracketed_zone() {
        let z: Zoned = "2026-08-18T09:05:00.123-05:00[America/Chicago]"
            .parse()
            .unwrap();
        let s = to_wire(&z);
        assert!(s.contains("[America/Chicago]"), "{s}");
        assert!(s.starts_with("2026-08-18T09:05:00.123-05:00"), "{s}");
        // round-trip
        assert_eq!(parse_wire(&s).unwrap(), z);
    }

    #[test]
    fn plus_seconds_advances_and_keeps_zone() {
        let z: Zoned = "2026-08-18T09:00:00-05:00[America/Chicago]".parse().unwrap();
        let later = plus_seconds(&z, 300.0);
        assert_eq!(to_wire(&later), "2026-08-18T09:05:00-05:00[America/Chicago]");
        let frac = plus_seconds(&z, 0.5);
        assert!(to_wire(&frac).starts_with("2026-08-18T09:00:00.5"));
    }
}

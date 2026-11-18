//! Hex encoding for binary fields in a profile.
//!
//! Profiles should be readable when debugging and easy to diff across
//! PVXS versions with `mimus inspect`. Hex strings are more readable
//! than JSON arrays of integers, easy to diff, and require no
//! additional dependency.

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
        s.push(char::from_digit((b & 0x0F) as u32, 16).expect("nibble"));
    }
    s
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("hex string has odd length {}", s.len()));
    }
    // The length is even, so every byte belongs to a pair.
    let (pairs, _) = s.as_bytes().as_chunks::<2>();
    let mut out = Vec::with_capacity(pairs.len());
    for &[h, l] in pairs {
        let hi = (h as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit {:?}", h as char))?;
        let lo = (l as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex digit {:?}", l as char))?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

/// Serde adapter for `Vec<u8>`, encoded as a hex string.
pub mod bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Serde adapter for `Option<Vec<u8>>`, encoded as a hex string or `null`.
pub mod opt_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&super::encode(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| super::decode(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Serde adapter for `Vec<Vec<u8>>`, encoding each inner vector as a hex string.
pub mod vec_of_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for item in v {
            seq.serialize_element(&super::encode(item))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        let raw = Vec::<String>::deserialize(d)?;
        raw.into_iter()
            .map(|s| super::decode(&s).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for case in [
            vec![],
            vec![0x00],
            vec![0xFF, 0x00, 0xA5],
            (0u8..=255).collect(),
        ] {
            assert_eq!(decode(&encode(&case)).unwrap(), case);
        }
    }

    #[test]
    fn encodes_lowercase_with_leading_zeroes() {
        assert_eq!(encode(&[0x0A, 0xB0]), "0ab0");
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode("abc").is_err());
        assert!(decode("zz").is_err());
    }
}

//! Compact DID representation.
//!
//! Almost every account on the network is a `did:plc:` whose identifier is
//! exactly 24 base32 characters (120 bits). Storing those as 15 raw bytes
//! makes equality a 15-byte compare, hashing trivial, and large DID sets
//! (thousands of entries) cheap to probe on every event.

use std::fmt;

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

const fn decode_table() -> [u8; 256] {
    let mut t = [0xffu8; 256];
    let mut i = 0;
    while i < 32 {
        t[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    t
}
static DECODE: [u8; 256] = decode_table();

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Did {
    /// `did:plc:` + 24 base32 chars, packed.
    Plc([u8; 15]),
    /// Anything else (`did:web:…`, malformed input), stored verbatim.
    Other(Box<str>),
}

impl Did {
    pub fn parse(s: &str) -> Did {
        if let Some(id) = s.strip_prefix("did:plc:") {
            if let Some(packed) = pack_plc(id.as_bytes()) {
                return Did::Plc(packed);
            }
        }
        Did::Other(s.into())
    }

    pub fn is_plc(&self) -> bool {
        matches!(self, Did::Plc(_))
    }

    /// Length of the string form in bytes.
    pub fn str_len(&self) -> usize {
        match self {
            Did::Plc(_) => 32,
            Did::Other(s) => s.len(),
        }
    }

    /// Write the string form into `out`.
    pub fn push_to(&self, out: &mut String) {
        match self {
            Did::Plc(p) => {
                out.push_str("did:plc:");
                let chars = unpack_plc(p);
                // SAFETY-free: alphabet is ASCII.
                out.push_str(std::str::from_utf8(&chars).unwrap_or_default());
            }
            Did::Other(s) => out.push_str(s),
        }
    }

    /// True if `s` is the string form of this DID (no allocation).
    pub fn eq_str(&self, s: &str) -> bool {
        match self {
            Did::Plc(p) => match s.strip_prefix("did:plc:") {
                Some(id) => pack_plc(id.as_bytes()).is_some_and(|q| &q == p),
                None => false,
            },
            Did::Other(o) => &**o == s,
        }
    }
}

fn pack_plc(id: &[u8]) -> Option<[u8; 15]> {
    if id.len() != 24 {
        return None;
    }
    let mut out = [0u8; 15];
    for (chunk, o) in id.chunks_exact(8).zip(out.chunks_exact_mut(5)) {
        let mut acc = 0u64;
        for &c in chunk {
            let v = DECODE[c as usize];
            if v == 0xff {
                return None;
            }
            acc = (acc << 5) | v as u64;
        }
        o.copy_from_slice(&acc.to_be_bytes()[3..8]);
    }
    Some(out)
}

fn unpack_plc(p: &[u8; 15]) -> [u8; 24] {
    let mut out = [0u8; 24];
    for (chunk, o) in p.chunks_exact(5).zip(out.chunks_exact_mut(8)) {
        let mut buf = [0u8; 8];
        buf[3..8].copy_from_slice(chunk);
        let acc = u64::from_be_bytes(buf);
        for (i, c) in o.iter_mut().enumerate() {
            *c = ALPHABET[((acc >> (35 - 5 * i)) & 31) as usize];
        }
    }
    out
}

impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Did::Plc(p) => {
                let chars = unpack_plc(p);
                f.write_str("did:plc:")?;
                f.write_str(std::str::from_utf8(&chars).unwrap_or_default())
            }
            Did::Other(s) => f.write_str(s),
        }
    }
}

impl fmt::Debug for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Lowercase RFC 4648 base32 without padding (multibase `b` payload).
pub fn base32_lower(data: &[u8], out: &mut String) {
    let mut acc = 0u32;
    let mut bits = 0;
    for &b in data {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
}

/// [`base32_lower`] into a byte buffer, five input bytes (eight characters)
/// at a time. The output is ASCII.
pub fn base32_lower_bytes(data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len().div_ceil(5) * 8);
    let mut chunks = data.chunks_exact(5);
    for c in &mut chunks {
        let acc = u64::from_be_bytes([0, 0, 0, c[0], c[1], c[2], c[3], c[4]]);
        let mut o = [0u8; 8];
        for (i, o) in o.iter_mut().enumerate() {
            *o = ALPHABET[((acc >> (35 - 5 * i)) & 31) as usize];
        }
        out.extend_from_slice(&o);
    }
    let mut acc = 0u32;
    let mut bits = 0;
    for &b in chunks.remainder() {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plc_round_trip() {
        for s in [
            "did:plc:mcc5yghbk7tixqryac2yxku7",
            "did:plc:aaaaaaaaaaaaaaaaaaaaaaaa",
            "did:plc:777777777777777777777777",
            "did:plc:z72i7hdynmk6r22z27h6tvur",
        ] {
            let d = Did::parse(s);
            assert!(d.is_plc(), "{s}");
            assert_eq!(d.to_string(), s);
            assert!(d.eq_str(s));
            let mut buf = String::new();
            d.push_to(&mut buf);
            assert_eq!(buf, s);
        }
    }

    #[test]
    fn non_plc_is_verbatim() {
        for s in ["did:web:example.com", "did:plc:short", "did:plc:UPPERCASEUPPERCASEUPPERC", "", "did:plc:mcc5yghbk7tixqryac2yxku1"] {
            let d = Did::parse(s);
            assert!(!d.is_plc(), "{s}");
            assert_eq!(d.to_string(), s);
            assert!(d.eq_str(s));
        }
    }

    #[test]
    fn distinct_dids_differ() {
        assert_ne!(Did::parse("did:plc:mcc5yghbk7tixqryac2yxku7"), Did::parse("did:plc:mcc5yghbk7tixqryac2yxku6"));
        assert!(!Did::parse("did:plc:mcc5yghbk7tixqryac2yxku7").eq_str("did:plc:mcc5yghbk7tixqryac2yxku6"));
    }

    #[test]
    fn base32_matches_known_vectors() {
        let mut s = String::new();
        base32_lower(b"foobar", &mut s);
        assert_eq!(s, "mzxw6ytboi");
        s.clear();
        base32_lower(b"f", &mut s);
        assert_eq!(s, "my");
    }

    #[test]
    fn base32_bytes_matches_string_encoder() {
        let mut rng = fastrand::Rng::with_seed(32);
        for len in 0..80 {
            let data: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
            let mut want = String::new();
            base32_lower(&data, &mut want);
            let mut got = b"x".to_vec();
            base32_lower_bytes(&data, &mut got);
            assert_eq!(&got[1..], want.as_bytes(), "len {len}");
        }
    }

    #[test]
    fn size_is_small() {
        assert!(std::mem::size_of::<Did>() <= 24);
    }
}

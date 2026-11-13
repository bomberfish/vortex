//! CAR v1 block index.
//!
//! Firehose commits ship their changed blocks as an embedded CAR file. Only
//! random access by CID is needed, so the archive is indexed in place as
//! `(cid bytes, data)` slices with no copying and no hashing.

use super::cbor::read_varint;
use super::DecodeError;

pub struct Car<'a> {
    blocks: Vec<(&'a [u8], &'a [u8])>,
}

/// Byte length of the binary CID at the start of `buf`.
fn cid_len(buf: &[u8]) -> Result<usize, DecodeError> {
    // CIDv0: a bare sha2-256 multihash.
    if buf.len() >= 34 && buf[0] == 0x12 && buf[1] == 0x20 {
        return Ok(34);
    }
    // CIDv1 whose version, codec, hash function and digest length each fit one
    // varint byte, which is every CID atproto produces (`01 71 12 20 …`).
    if let [1, codec, hash, digest, ..] = *buf {
        if (codec | hash | digest) < 0x80 {
            let total = 4 + digest as usize;
            return if total <= buf.len() { Ok(total) } else { Err(DecodeError::Cbor("truncated CID")) };
        }
    }
    let mut pos = 0;
    let version = read_varint(buf, &mut pos)?;
    if version != 1 {
        return Err(DecodeError::Cbor("unsupported CID version"));
    }
    let _codec = read_varint(buf, &mut pos)?;
    let _hash = read_varint(buf, &mut pos)?;
    let digest_len = read_varint(buf, &mut pos)?;
    let total = (pos as u64).checked_add(digest_len).filter(|&t| t <= buf.len() as u64);
    total.map(|t| t as usize).ok_or(DecodeError::Cbor("truncated CID"))
}

/// Visit every block of an archive, in order, as `(cid, data)`.
///
/// Fails if the archive is malformed anywhere; blocks before the damage have
/// been visited by then, so a caller that wants all-or-nothing must discard
/// what it collected.
pub fn for_each_block<'a>(buf: &'a [u8], mut f: impl FnMut(&'a [u8], &'a [u8])) -> Result<(), DecodeError> {
    let mut pos = 0;
    let header_len = read_varint(buf, &mut pos)?;
    pos = (pos as u64)
        .checked_add(header_len)
        .filter(|&p| p <= buf.len() as u64)
        .ok_or(DecodeError::Cbor("truncated CAR header"))? as usize;

    while pos < buf.len() {
        let len = read_varint(buf, &mut pos)?;
        let end = (pos as u64)
            .checked_add(len)
            .filter(|&e| e <= buf.len() as u64)
            .ok_or(DecodeError::Cbor("truncated CAR block"))? as usize;
        let section = &buf[pos..end];
        let cl = cid_len(section)?;
        f(&section[..cl], &section[cl..]);
        pos = end;
    }
    Ok(())
}

impl<'a> Car<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let mut blocks = Vec::with_capacity(16);
        for_each_block(buf, |cid, data| blocks.push((cid, data)))?;
        Ok(Car { blocks })
    }

    /// Block data for a binary CID.
    pub fn get(&self, cid: &[u8]) -> Option<&'a [u8]> {
        self.blocks.iter().find(|(c, _)| *c == cid).map(|(_, d)| *d)
    }

    /// Index of the block with this CID, in archive order.
    pub fn position(&self, cid: &[u8]) -> Option<usize> {
        self.blocks.iter().position(|(c, _)| *c == cid)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod testenc {
    pub fn varint(out: &mut Vec<u8>, mut n: u64) {
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    /// A syntactically valid CIDv1 (dag-cbor, sha2-256) with a fake digest.
    pub fn fake_cid(seed: u8) -> Vec<u8> {
        let mut c = vec![1, 0x71, 0x12, 0x20];
        c.extend([seed; 32]);
        c
    }

    pub fn car(blocks: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let header = [0xa0u8]; // empty map; contents are irrelevant to the reader
        let mut out = Vec::new();
        varint(&mut out, header.len() as u64);
        out.extend(header);
        for (cid, data) in blocks {
            varint(&mut out, (cid.len() + data.len()) as u64);
            out.extend(cid);
            out.extend(data);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testenc::*;
    use super::*;

    #[test]
    fn indexes_blocks() {
        let blocks = vec![(fake_cid(1), b"one".to_vec()), (fake_cid(2), vec![0u8; 300]), (fake_cid(3), vec![])];
        let bytes = car(&blocks);
        let c = Car::parse(&bytes).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c.get(&fake_cid(1)), Some(&b"one"[..]));
        assert_eq!(c.get(&fake_cid(2)).unwrap().len(), 300);
        assert_eq!(c.get(&fake_cid(3)), Some(&[][..]));
        assert_eq!(c.get(&fake_cid(4)), None);
    }

    #[test]
    fn for_each_block_visits_in_order_and_fails_like_parse() {
        let blocks = vec![(fake_cid(1), b"one".to_vec()), (fake_cid(2), vec![7u8; 300]), (fake_cid(1), b"dup".to_vec())];
        let bytes = car(&blocks);
        let mut seen = Vec::new();
        for_each_block(&bytes, |c, d| seen.push((c.to_vec(), d.to_vec()))).unwrap();
        assert_eq!(seen, blocks);
        for cut in 0..bytes.len() {
            let mut n = 0;
            let walked = for_each_block(&bytes[..cut], |_, _| n += 1);
            let parsed = Car::parse(&bytes[..cut]);
            assert_eq!(walked.is_ok(), parsed.is_ok(), "cut {cut}");
            if let Ok(c) = parsed {
                assert_eq!(c.len(), n);
            }
        }
    }

    #[test]
    fn cid_lengths() {
        // CIDv1 with single-byte fields, CIDv0, multi-byte varint fields,
        // and the ways each can be cut short.
        let v1 = fake_cid(3);
        assert_eq!(cid_len(&v1).unwrap(), 36);
        assert_eq!(cid_len(&[&v1[..], b"data"].concat()).unwrap(), 36);
        assert!(cid_len(&v1[..35]).is_err());
        let mut v0 = vec![0x12, 0x20];
        v0.extend([5u8; 32]);
        assert_eq!(cid_len(&v0).unwrap(), 34);
        assert!(cid_len(&v0[..33]).is_err());
        // Codec 0x0200 (two varint bytes), 4-byte digest.
        let wide = [1, 0x80, 0x04, 0x12, 4, 9, 9, 9, 9];
        assert_eq!(cid_len(&wide).unwrap(), 9);
        assert!(cid_len(&wide[..8]).is_err());
        // Digest length as a two-byte varint (0x80 0x01 = 128).
        let mut long = vec![1, 0x71, 0x13, 0x80, 0x01];
        long.extend([0u8; 128]);
        assert_eq!(cid_len(&long).unwrap(), 133);
        for bad in [&[][..], &[1], &[1, 0x71], &[1, 0x71, 0x12], &[2, 0x71, 0x12, 0x20], &[0, 0, 0, 0], &[1, 0x71, 0x12, 0x7f]] {
            assert!(cid_len(bad).is_err(), "{bad:x?}");
        }
        assert_eq!(cid_len(&[1, 0x71, 0x12, 0]).unwrap(), 4);
    }

    #[test]
    fn truncated_archives_error() {
        let bytes = car(&[(fake_cid(1), b"data".to_vec())]);
        for cut in 0..bytes.len() {
            let r = Car::parse(&bytes[..cut]);
            if cut > 2 {
                assert!(r.is_err(), "cut {cut}");
            }
        }
        assert!(Car::parse(&[0xff; 12]).is_err());
    }
}

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

impl<'a> Car<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let mut pos = 0;
        let header_len = read_varint(buf, &mut pos)?;
        pos = (pos as u64)
            .checked_add(header_len)
            .filter(|&p| p <= buf.len() as u64)
            .ok_or(DecodeError::Cbor("truncated CAR header"))? as usize;

        let mut blocks = Vec::with_capacity(16);
        while pos < buf.len() {
            let len = read_varint(buf, &mut pos)?;
            let end = (pos as u64)
                .checked_add(len)
                .filter(|&e| e <= buf.len() as u64)
                .ok_or(DecodeError::Cbor("truncated CAR block"))? as usize;
            let section = &buf[pos..end];
            let cl = cid_len(section)?;
            blocks.push((&section[..cl], &section[cl..]));
            pos = end;
        }
        Ok(Car { blocks })
    }

    /// Block data for a binary CID.
    pub fn get(&self, cid: &[u8]) -> Option<&'a [u8]> {
        self.blocks.iter().find(|(c, _)| *c == cid).map(|(_, d)| *d)
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

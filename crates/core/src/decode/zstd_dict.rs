//! Dictionary-compressed zstd frames (Jetstream's optional compression).
//!
//! Each binary websocket message is one complete zstd frame compressed against
//! a shared dictionary. v1 uses a fixed legacy dictionary; v2 dictionaries are
//! identified by ID and may be retrained, so the v2 dictionary can be swapped
//! at runtime after fetching the server's current one.

use super::DecodeError;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Legacy `/subscribe` dictionary (ID 1612007021).
pub static LEGACY_DICT: &[u8] = include_bytes!("../../../../assets/dict/legacy_subscribe.zdict");
/// Bundled `subscribeEvents` dictionary; servers may have moved on, in which
/// case the current one is fetched via `getZstdDictionary`.
pub static BUNDLED_V2_DICT: &[u8] = include_bytes!("../../../../assets/dict/subscribe_events_20260811.zdict");

const ZSTD_DICT_MAGIC: u32 = 0xEC30_A437;
/// Refuse to inflate a single frame past this size.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The dictionary ID embedded in a structured zstd dictionary's header.
pub fn dict_id(dict: &[u8]) -> Option<u32> {
    if dict.len() < 8 || u32::from_le_bytes(dict[0..4].try_into().ok()?) != ZSTD_DICT_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes(dict[4..8].try_into().ok()?))
}

/// Shared, swappable dictionary set.
pub struct Dictionaries {
    v2: RwLock<Arc<[u8]>>,
    generation: AtomicU64,
}

impl Default for Dictionaries {
    fn default() -> Self {
        Dictionaries { v2: RwLock::new(Arc::from(BUNDLED_V2_DICT)), generation: AtomicU64::new(0) }
    }
}

impl Dictionaries {
    pub fn v1(&self) -> Arc<[u8]> {
        Arc::from(LEGACY_DICT)
    }

    pub fn v2(&self) -> Arc<[u8]> {
        self.v2.read().clone()
    }

    pub fn v2_id(&self) -> u32 {
        dict_id(&self.v2.read()).unwrap_or(0)
    }

    /// Install a newly fetched v2 dictionary. Decoders pick it up on their
    /// next frame.
    pub fn set_v2(&self, dict: Vec<u8>) -> Result<u32, DecodeError> {
        let id = dict_id(&dict).ok_or_else(|| DecodeError::Zstd("not a structured zstd dictionary".into()))?;
        *self.v2.write() = Arc::from(dict);
        self.generation.fetch_add(1, Ordering::Release);
        Ok(id)
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

pub struct DictDecompressor {
    inner: zstd::bulk::Decompressor<'static>,
}

impl DictDecompressor {
    pub fn new(dict: &[u8]) -> Result<Self, DecodeError> {
        let inner = zstd::bulk::Decompressor::with_dictionary(dict).map_err(|e| DecodeError::Zstd(e.to_string()))?;
        Ok(DictDecompressor { inner })
    }

    /// Decompress one frame into `out` (cleared first, capacity reused).
    pub fn decompress(&mut self, src: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
        out.clear();
        let hint = match zstd::zstd_safe::get_frame_content_size(src) {
            Ok(Some(n)) => {
                let n = usize::try_from(n).unwrap_or(usize::MAX);
                if n > MAX_FRAME_BYTES {
                    return Err(DecodeError::Zstd("frame too large".into()));
                }
                Some(n)
            }
            Ok(None) => None,
            Err(_) => return Err(DecodeError::Zstd("not a zstd frame".into())),
        };
        let mut cap = hint.unwrap_or_else(|| (src.len() * 8).max(4096));
        loop {
            out.clear();
            out.reserve(cap);
            match self.inner.decompress_to_buffer(src, out) {
                Ok(_) => return Ok(()),
                // Without a declared content size the guess may be too small.
                Err(_) if hint.is_none() && cap < MAX_FRAME_BYTES => cap *= 4,
                Err(e) => return Err(DecodeError::Zstd(e.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_dictionaries_have_expected_ids() {
        assert_eq!(dict_id(LEGACY_DICT), Some(1_612_007_021));
        assert_eq!(dict_id(BUNDLED_V2_DICT), Some(20_260_811));
        assert_eq!(dict_id(b"nope"), None);
    }

    #[test]
    fn round_trip_with_dictionary() {
        let msg = br#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#commit","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":1}}"#;
        for dict in [LEGACY_DICT, BUNDLED_V2_DICT] {
            let mut enc = zstd::bulk::Compressor::with_dictionary(3, dict).unwrap();
            let packed = enc.compress(msg).unwrap();
            assert!(packed.len() < msg.len());
            let mut dec = DictDecompressor::new(dict).unwrap();
            let mut out = Vec::new();
            dec.decompress(&packed, &mut out).unwrap();
            assert_eq!(out, msg);
            // Reuse with a smaller buffer requirement.
            dec.decompress(&packed, &mut out).unwrap();
            assert_eq!(out, msg);
        }
    }

    #[test]
    fn garbage_is_an_error() {
        let mut dec = DictDecompressor::new(BUNDLED_V2_DICT).unwrap();
        let mut out = Vec::new();
        assert!(dec.decompress(b"definitely not zstd", &mut out).is_err());
        assert!(dec.decompress(&[], &mut out).is_err());
    }

    #[test]
    fn swapping_bumps_generation() {
        let d = Dictionaries::default();
        assert_eq!(d.v2_id(), 20_260_811);
        let g = d.generation();
        assert!(d.set_v2(b"junk".to_vec()).is_err());
        assert_eq!(d.generation(), g);
        assert_eq!(d.set_v2(LEGACY_DICT.to_vec()).unwrap(), 1_612_007_021);
        assert_eq!(d.generation(), g + 1);
    }
}

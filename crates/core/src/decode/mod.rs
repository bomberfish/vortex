//! Wire frames → normalized [`Event`]s.

pub mod car;
pub mod cbor;
pub mod firehose;
pub mod frame;
pub mod streams;
pub mod zstd_dict;

use crate::event::{Event, EventBuilder};
use crate::json::JsonError;
use std::sync::Arc;
use zstd_dict::{DictDecompressor, Dictionaries};

/// Which protocol a frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wire {
    /// `network.bsky.jetstream.subscribeEvents` (JSON, `xrpc.v1.json` framing).
    JetstreamV2,
    /// Legacy `/subscribe` (JSON).
    JetstreamV1,
    /// `com.atproto.sync.subscribeRepos` (DAG-CBOR + CAR).
    Firehose,
    /// Spacedust link stream (JSON).
    Spacedust,
    /// plc.directory operation stream (JSON).
    Plc,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("json: {0}")]
    Json(#[from] JsonError),
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
    #[error("frame is not valid UTF-8")]
    Utf8,
    #[error("zstd: {0}")]
    Zstd(String),
    #[error("cbor: {0}")]
    Cbor(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Error,
}

/// An out-of-band message from the server (`#info` advisory or error frame).
#[derive(Debug, Clone)]
pub struct Notice {
    pub level: NoticeLevel,
    pub name: String,
    pub message: String,
}

#[derive(Debug)]
pub enum Decoded {
    Event(Event),
    Notice(Notice),
}

/// Extra per-event data the pipeline needs but that is not worth storing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meta {
    /// Portable (cross-instance) resume timestamp, or 0.
    pub witnessed_us: i64,
}

/// Per-thread decode state: builder scratch, decompression contexts and
/// transcode buffers, all reused across frames.
pub struct FrameDecoder {
    builder: EventBuilder,
    dicts: Arc<Dictionaries>,
    dict_gen: u64,
    zstd_v1: Option<DictDecompressor>,
    zstd_v2: Option<DictDecompressor>,
    inflate: Vec<u8>,
    /// Firehose events are assembled as bytes and validated once, whole.
    transcode: Vec<u8>,
}

impl FrameDecoder {
    pub fn new(dicts: Arc<Dictionaries>) -> Self {
        FrameDecoder {
            builder: EventBuilder::new(),
            dict_gen: dicts.generation(),
            dicts,
            zstd_v1: None,
            zstd_v2: None,
            inflate: Vec::with_capacity(16 * 1024),
            transcode: Vec::with_capacity(16 * 1024),
        }
    }

    pub fn dictionaries(&self) -> &Arc<Dictionaries> {
        &self.dicts
    }

    /// Decode one websocket message, appending results to `out`.
    ///
    /// Returns the witnessed timestamp of the last event produced (0 if
    /// unknown). A firehose commit can yield several events (one per op).
    pub fn decode(
        &mut self,
        wire: Wire,
        payload: &[u8],
        is_text: bool,
        out: &mut Vec<Decoded>,
    ) -> Result<Meta, DecodeError> {
        match wire {
            Wire::Firehose => {
                firehose::decode_firehose_frame(payload, &mut self.builder, &mut self.transcode, out)?;
                Ok(Meta::default())
            }
            Wire::Spacedust | Wire::Plc => {
                let text = std::str::from_utf8(payload).map_err(|_| DecodeError::Utf8)?;
                let decoded = if wire == Wire::Plc {
                    streams::decode_plc_frame(text, &mut self.builder)?
                } else {
                    streams::decode_spacedust_frame(text, &mut self.builder)?
                };
                out.push(decoded);
                Ok(Meta::default())
            }
            Wire::JetstreamV1 | Wire::JetstreamV2 => {
                let text: &str = if is_text {
                    std::str::from_utf8(payload).map_err(|_| DecodeError::Utf8)?
                } else {
                    // Binary jetstream frames are dictionary-compressed zstd.
                    let gen = self.dicts.generation();
                    if gen != self.dict_gen {
                        self.dict_gen = gen;
                        self.zstd_v2 = None;
                    }
                    let slot = if wire == Wire::JetstreamV1 { &mut self.zstd_v1 } else { &mut self.zstd_v2 };
                    let dec = match slot {
                        Some(d) => d,
                        None => {
                            let dict = if wire == Wire::JetstreamV1 { self.dicts.v1() } else { self.dicts.v2() };
                            slot.insert(DictDecompressor::new(&dict)?)
                        }
                    };
                    dec.decompress(payload, &mut self.inflate)?;
                    std::str::from_utf8(&self.inflate).map_err(|_| DecodeError::Utf8)?
                };
                let decoded = frame::decode_json_frame(text, &mut self.builder)?;
                let meta = Meta { witnessed_us: self.builder.witnessed_us };
                out.push(decoded);
                Ok(meta)
            }
        }
    }
}

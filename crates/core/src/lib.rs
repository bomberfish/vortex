//! vortex-core: decode, filter and buffer AT Protocol event streams.
//!
//! The crate is UI-agnostic. Data flows one way:
//!
//! ```text
//! net (tokio, I/O only) ──frames──▶ pipeline thread ──events──▶ Store ◀── UI reads
//!                                   decompress · decode · filter
//! ```

pub mod decode;
pub mod did;
pub mod event;
pub mod json;
pub mod nsid;
pub mod time;

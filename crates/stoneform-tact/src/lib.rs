//! Blizzard's TACT (CDN) and CASC (local storage) formats, sans I/O.
//!
//! Every codec here works on borrowed bytes (`&[u8]`, ideally a memory-mapped
//! file), on `impl Read` / `impl Write`, and returns plain values: no
//! filesystem, network, threads or async. Fetching, caching and writing files
//! is the adapters' job; this crate only knows the bytes.
//!
//! WoW's manifests are large (about 3 million download entries, a 110 MB
//! encoding table), so the big ones are read in place from the borrowed bytes
//! instead of being copied into collections.
//!
//! Ported from Waypoint (0BSD). Format references: <https://wowdev.wiki/TACT>
//! and <https://wowdev.wiki/CASC>.

pub mod archive_group;
pub mod archive_index;
pub mod blte;
pub mod bpsv;
pub mod build_info;
mod bytes;
pub mod casc;
pub mod config;
pub mod download;
pub mod encoding;
mod error;
pub mod install;
pub mod keys;
pub mod languages;
pub mod lookup3;
pub mod product_config;
pub mod product_db;
pub mod protobuf;
pub mod tags;

pub use error::{Error, Result};
pub use keys::{ContentKey, EncodingKey, ParseKeyError, TruncatedKey};

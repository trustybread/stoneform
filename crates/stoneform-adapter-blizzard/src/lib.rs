//! Adapters for Blizzard's services: the version service and the CDN, over HTTPS.
//!
//! `BlizzardHttp` moves bytes: URLs, mirrors, byte ranges and retries.
//! `HttpVersionSource` and `HttpContentDelivery` implement the core's ports on
//! top of it, with `stoneform-tact` for the formats. Retry and mirror rules
//! follow Waypoint's `NetworkRetry.swift` and `CDN.swift` (0BSD).

mod cdn;
mod client;
mod error;
mod retry;
mod versions;

pub use cdn::HttpContentDelivery;
pub use client::{BlizzardHttp, VersionTable};
pub use error::HttpError;
pub use retry::Retry;
pub use versions::HttpVersionSource;

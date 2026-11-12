//! StoneForm's use cases and the driven ports they depend on.
//!
//! The ports are traits this crate owns; adapters implement them. Nothing in
//! here names an adapter, touches the network, the disk or a macOS API.

pub mod ports;
mod session;

pub use session::{AppError, Session, SignedIn};

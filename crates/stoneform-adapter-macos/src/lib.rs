//! macOS adapters for StoneForm's driven ports.

mod handoff;
mod keychain;
mod process;

pub use handoff::{BATTLE_NET_DOMAIN, PrefsTokenHandoff};
pub use keychain::KeychainSecretStore;
pub use process::MacGameProcess;

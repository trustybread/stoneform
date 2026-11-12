//! StoneForm's core types and rules.
//!
//! Everything here is a plain value: no I/O, no async, no macOS types, so every
//! rule is unit-testable on its own.

mod autopilot;
mod locale;
mod login;
mod product;
mod region;
mod token;

pub use autopilot::{AutopilotFacts, AutopilotStep, next_step};
pub use locale::{Locale, ParseLocaleError};
pub use login::{login_url, token_from_callback};
pub use product::{GameCodename, Product, WOW_FOREVER};
pub use region::{ParseRegionError, Region};
pub use token::{AccountId, LaunchToken, ParseAccountIdError, ParseTokenError};

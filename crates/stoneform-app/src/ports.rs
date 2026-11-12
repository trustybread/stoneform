//! Driven ports: what the core needs from the outside world.

use std::error::Error as StdError;
use std::future::Future;

use stoneform_domain::{AccountId, GameCodename, LaunchToken, Region};

type BoxError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("sign-in was cancelled")]
    Cancelled,
    #[error("sign-in failed: {0}")]
    Failed(String),
}

/// Shows Blizzard's web login and returns the launch token it hands out.
pub trait LoginFlow: Send + Sync {
    /// Resolves with the token from the login page's final redirect, or with
    /// [`LoginError::Cancelled`] when the user closes the window.
    fn acquire(&self, region: Region) -> impl Future<Output = Result<LaunchToken, LoginError>> + Send;

    /// Forgets the web session (cookies) kept for `account`.
    fn forget(&self, account: &AccountId) -> impl Future<Output = Result<(), LoginError>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("could not access saved sign-in: {0}")]
pub struct SecretError(#[source] pub BoxError);

/// Keeps launch tokens, one per account and codename.
pub trait SecretStore: Send + Sync {
    fn token(&self, account: &AccountId, codename: GameCodename) -> Result<Option<LaunchToken>, SecretError>;
    fn save(&self, token: &LaunchToken, codename: GameCodename) -> Result<(), SecretError>;
    /// Removes every token of `account`; succeeds if there were none.
    fn forget(&self, account: &AccountId) -> Result<(), SecretError>;
}

/// What StoneForm remembers between runs. Never secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub region: Option<Region>,
    pub account: Option<AccountId>,
}

#[derive(Debug, thiserror::Error)]
#[error("could not access settings: {0}")]
pub struct SettingsError(#[source] pub BoxError);

pub trait SettingsStore: Send + Sync {
    /// The saved settings, or the defaults when nothing was saved yet.
    fn load(&self) -> Result<Settings, SettingsError>;
    fn save(&self, settings: &Settings) -> Result<(), SettingsError>;
}

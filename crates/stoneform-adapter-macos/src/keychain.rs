use std::collections::BTreeMap;

use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};
use stoneform_app::ports::{SecretError, SecretStore};
use stoneform_domain::{AccountId, GameCodename, LaunchToken};

/// `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = -25300;

/// Launch tokens in the login keychain: one generic-password item per account,
/// holding a JSON map of codename to token (the layout Waypoint uses too).
#[derive(Debug, Clone)]
pub struct KeychainSecretStore {
    service: String,
}

impl KeychainSecretStore {
    pub const SERVICE: &'static str = "StoneForm launch tokens";

    pub fn new() -> Self {
        Self::with_service(Self::SERVICE)
    }

    /// A separate service name, so tests never touch real sign-ins.
    pub fn with_service(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn read(&self, account: &AccountId) -> Result<BTreeMap<String, String>, SecretError> {
        match get_generic_password(&self.service, account.as_str()) {
            Ok(data) => serde_json::from_slice(&data).map_err(|error| SecretError(error.into())),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(BTreeMap::new()),
            Err(error) => Err(SecretError(error.into())),
        }
    }

    fn write(&self, account: &AccountId, tokens: &BTreeMap<String, String>) -> Result<(), SecretError> {
        let data = serde_json::to_vec(tokens).map_err(|error| SecretError(error.into()))?;
        set_generic_password(&self.service, account.as_str(), &data).map_err(|error| SecretError(error.into()))
    }
}

impl Default for KeychainSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeychainSecretStore {
    fn token(&self, account: &AccountId, codename: GameCodename) -> Result<Option<LaunchToken>, SecretError> {
        let tokens = self.read(account)?;
        let Some(value) = tokens.get(codename.as_str()) else { return Ok(None) };
        match LaunchToken::parse(value) {
            Ok(token) => Ok(Some(token)),
            Err(error) => {
                // Treat a damaged entry as missing: the next sign-in replaces it.
                tracing::warn!(%account, codename = codename.as_str(), %error, "ignoring an unreadable saved token");
                Ok(None)
            }
        }
    }

    fn save(&self, token: &LaunchToken, codename: GameCodename) -> Result<(), SecretError> {
        let account = token.account_id();
        let mut tokens = self.read(&account)?;
        tokens.insert(codename.as_str().to_owned(), token.expose().to_owned());
        self.write(&account, &tokens)
    }

    fn forget(&self, account: &AccountId) -> Result<(), SecretError> {
        match delete_generic_password(&self.service, account.as_str()) {
            Err(error) if error.code() != ITEM_NOT_FOUND => Err(SecretError(error.into())),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real login keychain (under a test-only service), so it is
    /// opt-in: `cargo test -p stoneform-adapter-macos -- --ignored`.
    #[test]
    #[ignore = "uses the login keychain"]
    fn round_trips_tokens_through_the_keychain() {
        let store = KeychainSecretStore::with_service("StoneForm tests");
        let token = LaunchToken::parse("EU-0123456789abcdef0123456789abcdef-1000000001").unwrap();
        let account = token.account_id();
        store.forget(&account).unwrap();

        assert_eq!(store.token(&account, GameCodename::WOW).unwrap(), None);
        store.save(&token, GameCodename::WOW).unwrap();
        assert_eq!(store.token(&account, GameCodename::WOW).unwrap(), Some(token));
        store.forget(&account).unwrap();
        assert_eq!(store.token(&account, GameCodename::WOW).unwrap(), None);
        store.forget(&account).unwrap();
    }
}

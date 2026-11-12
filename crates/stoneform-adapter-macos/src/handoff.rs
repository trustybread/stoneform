//! The `TokenHandoff` port: the launch token in the `net.battle` preferences.
//!
//! On macOS, Battle.net doesn't pass the login to a game on the command line.
//! It writes the token, encrypted, to `Launch Options/<codename>/WEB_TOKEN` in
//! the `net.battle` domain and starts the game, which reads it back (the
//! client's `RegistryDarwin`). StoneForm writes the same values. Ported from
//! Waypoint's `LaunchOptions.swift` (0BSD).

use std::ffi::{c_char, c_int, c_void};

use objc2::AnyThread;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSData, NSString, NSUserDefaults, NSUserName};
use stoneform_app::ports::{SystemError, TokenHandoff};
use stoneform_domain::{GameCodename, LaunchToken, Locale, Region};

/// The preferences domain Battle.net and its games share.
pub const BATTLE_NET_DOMAIN: &str = "net.battle";

/// Writes the hand-off into a preferences domain, `net.battle` by default.
#[derive(Debug, Clone)]
pub struct PrefsTokenHandoff {
    domain: String,
    user_name: String,
}

#[derive(Debug, thiserror::Error)]
enum HandoffError {
    #[error("cannot open the {0} preferences")]
    Domain(String),
    #[error("could not save the {0} preferences")]
    Synchronize(String),
    #[error("CommonCrypto failed with status {0}")]
    Crypto(i32),
    #[error("the decrypted token is not UTF-8")]
    NotUtf8,
}

impl PrefsTokenHandoff {
    /// The real hand-off: `net.battle`, keyed to the current macOS user.
    pub fn new() -> Self {
        Self::with_domain(BATTLE_NET_DOMAIN, NSUserName().to_string())
    }

    /// Another domain or user, for tests: they must never touch `net.battle`.
    pub fn with_domain(domain: impl Into<String>, user_name: impl Into<String>) -> Self {
        Self { domain: domain.into(), user_name: user_name.into() }
    }

    fn defaults(&self) -> Result<objc2::rc::Retained<NSUserDefaults>, HandoffError> {
        NSUserDefaults::initWithSuiteName(NSUserDefaults::alloc(), Some(&NSString::from_str(&self.domain)))
            .ok_or_else(|| HandoffError::Domain(self.domain.clone()))
    }

    fn key(codename: GameCodename, name: &str) -> objc2::rc::Retained<NSString> {
        NSString::from_str(&format!("Launch Options/{}/{name}", codename.as_str()))
    }

    /// The token stored for `codename`, decrypted; for tests and diagnostics.
    pub fn stored_token(&self, codename: GameCodename) -> Result<Option<String>, SystemError> {
        let defaults = self.defaults().map_err(SystemError::new)?;
        let Some(data) = defaults.dataForKey(&Self::key(codename, "WEB_TOKEN")) else { return Ok(None) };
        let token = TokenCipher::new(&self.user_name).decrypt(&data.to_vec()).map_err(SystemError::new)?;
        Ok(Some(token))
    }

    /// Removes what `write` stored for `codename`.
    pub fn clear(&self, codename: GameCodename) -> Result<(), SystemError> {
        let defaults = self.defaults().map_err(SystemError::new)?;
        for name in ["WEB_TOKEN", "REGION", "LOCALE"] {
            defaults.removeObjectForKey(&Self::key(codename, name));
        }
        defaults.synchronize();
        Ok(())
    }
}

impl Default for PrefsTokenHandoff {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenHandoff for PrefsTokenHandoff {
    fn write(
        &self,
        codename: GameCodename,
        token: &LaunchToken,
        region: Region,
        locale: Option<&Locale>,
    ) -> Result<(), SystemError> {
        let encrypted = TokenCipher::new(&self.user_name).encrypt(token.expose()).map_err(SystemError::new)?;
        let defaults = self.defaults().map_err(SystemError::new)?;
        let data = NSData::with_bytes(&encrypted);
        let data: &AnyObject = &data;
        let region_value = NSString::from_str(region.launch_option_value());
        let region_value: &AnyObject = &region_value;
        // SAFETY: NSData and NSString are property-list objects, as defaults require.
        unsafe {
            defaults.setObject_forKey(Some(data), &Self::key(codename, "WEB_TOKEN"));
            defaults.setObject_forKey(Some(region_value), &Self::key(codename, "REGION"));
        }
        // Only when known, so a language picked in Battle.net isn't clobbered.
        if let Some(locale) = locale {
            let locale_value = NSString::from_str(locale.as_str());
            let locale_value: &AnyObject = &locale_value;
            unsafe { defaults.setObject_forKey(Some(locale_value), &Self::key(codename, "LOCALE")) };
        }
        if !defaults.synchronize() {
            return Err(SystemError::new(HandoffError::Synchronize(self.domain.clone())));
        }
        Ok(())
    }
}

/// AES-128-CBC with a zero IV and PKCS#7 padding; the key is PBKDF2-HMAC-SHA1
/// (1000 rounds, salt `someSalt`) over a fixed 16-byte entropy XORed with the
/// macOS user name. Mirrors `RegistryDarwin` in the game's Battle.net SDK.
#[derive(Debug, Clone)]
struct TokenCipher {
    key: [u8; 16],
}

const ENTROPY: [u8; 16] = [200, 118, 244, 174, 76, 149, 46, 254, 242, 250, 15, 84, 25, 192, 156, 67];
const SALT: &[u8] = b"someSalt";
const ROUNDS: u32 = 1000;

// CommonCrypto, part of libSystem: the same primitives the game uses.
const K_CC_PBKDF2: u32 = 2;
const K_CC_PRF_HMAC_ALG_SHA1: u32 = 1;
const K_CC_ENCRYPT: u32 = 0;
const K_CC_DECRYPT: u32 = 1;
const K_CC_ALGORITHM_AES: u32 = 0;
const K_CC_OPTION_PKCS7_PADDING: u32 = 1;
const K_CC_SUCCESS: i32 = 0;
const AES_BLOCK: usize = 16;

unsafe extern "C" {
    fn CCKeyDerivationPBKDF(
        algorithm: u32,
        password: *const c_char,
        password_len: usize,
        salt: *const u8,
        salt_len: usize,
        prf: u32,
        rounds: u32,
        derived_key: *mut u8,
        derived_key_len: usize,
    ) -> c_int;

    fn CCCrypt(
        op: u32,
        alg: u32,
        options: u32,
        key: *const c_void,
        key_length: usize,
        iv: *const c_void,
        data_in: *const c_void,
        data_in_length: usize,
        data_out: *mut c_void,
        data_out_available: usize,
        data_out_moved: *mut usize,
    ) -> i32;
}

impl TokenCipher {
    fn new(user_name: &str) -> Self {
        let mut password = ENTROPY;
        // The SDK indexes the UTF-16 string and truncates each unit to a byte.
        for (byte, unit) in password.iter_mut().zip(user_name.encode_utf16()) {
            *byte ^= unit as u8;
        }
        let mut key = [0u8; 16];
        // SAFETY: every pointer is valid for the length passed with it.
        let status = unsafe {
            CCKeyDerivationPBKDF(
                K_CC_PBKDF2,
                password.as_ptr().cast(),
                password.len(),
                SALT.as_ptr(),
                SALT.len(),
                K_CC_PRF_HMAC_ALG_SHA1,
                ROUNDS,
                key.as_mut_ptr(),
                key.len(),
            )
        };
        debug_assert_eq!(status, 0, "PBKDF2 with valid lengths cannot fail");
        Self { key }
    }

    fn encrypt(&self, token: &str) -> Result<Vec<u8>, HandoffError> {
        self.crypt(K_CC_ENCRYPT, token.as_bytes())
    }

    fn decrypt(&self, data: &[u8]) -> Result<String, HandoffError> {
        String::from_utf8(self.crypt(K_CC_DECRYPT, data)?).map_err(|_| HandoffError::NotUtf8)
    }

    fn crypt(&self, op: u32, input: &[u8]) -> Result<Vec<u8>, HandoffError> {
        let iv = [0u8; AES_BLOCK];
        let mut output = vec![0u8; input.len() + AES_BLOCK];
        let mut moved = 0usize;
        // SAFETY: every pointer is valid for the length passed with it, and
        // `output` has room for a block of padding.
        let status = unsafe {
            CCCrypt(
                op,
                K_CC_ALGORITHM_AES,
                K_CC_OPTION_PKCS7_PADDING,
                self.key.as_ptr().cast(),
                self.key.len(),
                iv.as_ptr().cast(),
                input.as_ptr().cast(),
                input.len(),
                output.as_mut_ptr().cast(),
                output.len(),
                &mut moved,
            )
        };
        if status != K_CC_SUCCESS {
            return Err(HandoffError::Crypto(status));
        }
        output.truncate(moved);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "US-0123456789abcdef0123456789abcdef-123456789";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Waypoint's reference vector, computed independently with Python's
    /// `hashlib.pbkdf2_hmac` and `openssl enc -aes-128-cbc`.
    #[test]
    fn matches_the_reference_implementation() {
        let cipher = TokenCipher::new("testuser");
        let encrypted = cipher.encrypt(SAMPLE).unwrap();
        assert_eq!(
            hex(&encrypted),
            "eaf1d64a585191b2d1544912b0041d0d34b9f4506cc2337d83bf0acb22397d6a4164b9bce5b47562d2de8c1b5c638d92"
        );
        assert_eq!(cipher.decrypt(&encrypted).unwrap(), SAMPLE);
    }

    #[test]
    fn is_bound_to_the_user_name() {
        let encrypted = TokenCipher::new("alice").encrypt(SAMPLE).unwrap();
        let other = TokenCipher::new("bob").decrypt(&encrypted);
        assert!(other.is_err() || other.unwrap() != SAMPLE);
    }

    /// Writes real preferences, in a test-only domain: opt in with
    /// `cargo test -p stoneform-adapter-macos -- --ignored`.
    #[test]
    #[ignore = "writes user preferences"]
    fn writes_and_reads_back_the_hand_off() {
        let handoff = PrefsTokenHandoff::with_domain("com.stoneform.tests.handoff", "testuser");
        let token = LaunchToken::parse(SAMPLE).unwrap();
        let locale = Locale::parse("deDE").unwrap();

        handoff.write(GameCodename::WOW, &token, Region::Eu, Some(&locale)).unwrap();
        assert_eq!(handoff.stored_token(GameCodename::WOW).unwrap().as_deref(), Some(SAMPLE));
        let defaults = handoff.defaults().unwrap();
        let region = defaults.stringForKey(&PrefsTokenHandoff::key(GameCodename::WOW, "REGION"));
        assert_eq!(region.map(|r| r.to_string()).as_deref(), Some("EU"));

        handoff.clear(GameCodename::WOW).unwrap();
        assert_eq!(handoff.stored_token(GameCodename::WOW).unwrap(), None);
    }
}

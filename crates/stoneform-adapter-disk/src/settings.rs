use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use stoneform_app::ports::{Settings, SettingsError, SettingsStore};
use stoneform_domain::{AccountId, Region};

/// Settings in `~/Library/Application Support/StoneForm/settings.toml`.
#[derive(Debug, Clone)]
pub struct TomlSettingsStore {
    path: PathBuf,
}

/// The file's shape. Kept apart from [`Settings`] so the core needs no serde.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

impl TomlSettingsStore {
    /// The store at its standard place.
    pub fn new() -> Result<Self, SettingsError> {
        let base = directories::BaseDirs::new().ok_or_else(|| SettingsError("no home directory".into()))?;
        Ok(Self::at(base.data_dir().join("StoneForm").join("settings.toml")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SettingsStore for TomlSettingsStore {
    fn load(&self) -> Result<Settings, SettingsError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(error) => return Err(SettingsError(error.into())),
        };
        let file: SettingsFile = toml::from_str(&text).map_err(|error| SettingsError(error.into()))?;
        // A value that no longer parses is dropped rather than blocking start-up.
        let region = file.region.and_then(|value| {
            value.parse::<Region>().inspect_err(|error| tracing::warn!(%error, "ignoring the saved region")).ok()
        });
        let account = file.account.and_then(|value| {
            AccountId::parse(&value).inspect_err(|error| tracing::warn!(%error, "ignoring the saved account")).ok()
        });
        Ok(Settings { region, account })
    }

    fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        let file = SettingsFile {
            region: settings.region.map(|region| region.as_str().to_owned()),
            account: settings.account.as_ref().map(|account| account.as_str().to_owned()),
        };
        let text = toml::to_string_pretty(&file).map_err(|error| SettingsError(error.into()))?;
        write_atomically(&self.path, text.as_bytes()).map_err(|error| SettingsError(error.into()))
    }
}

/// Writes next to `path`, then renames over it, so a crash never leaves half a file.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("settings path has no parent"))?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension("toml.tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, TomlSettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TomlSettingsStore::at(dir.path().join("StoneForm").join("settings.toml"));
        (dir, store)
    }

    #[test]
    fn defaults_when_nothing_was_saved() {
        let (_dir, store) = store();
        assert_eq!(store.load().unwrap(), Settings::default());
    }

    #[test]
    fn round_trips_settings() {
        let (_dir, store) = store();
        let settings = Settings { region: Some(Region::Eu), account: Some(AccountId::parse("42").unwrap()) };

        store.save(&settings).unwrap();

        assert_eq!(store.load().unwrap(), settings);
        let text = fs::read_to_string(store.path()).unwrap();
        assert_eq!(text, "region = \"eu\"\naccount = \"42\"\n");
    }

    #[test]
    fn signed_out_settings_omit_the_account() {
        let (_dir, store) = store();
        store.save(&Settings { region: Some(Region::Us), account: None }).unwrap();
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "region = \"us\"\n");
    }

    #[test]
    fn drops_values_that_no_longer_parse() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "region = \"cn\"\naccount = \"not digits\"\n").unwrap();
        assert_eq!(store.load().unwrap(), Settings::default());
    }

    #[test]
    fn reports_a_broken_file() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "region = ").unwrap();
        assert!(store.load().is_err());
    }
}

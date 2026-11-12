//! The `InstallRecords` port: what tells the game, and Battle.net, which
//! build is installed. Ported from Waypoint's `CASCInstaller.writeBuildInfo`,
//! `WoWConfig.setLanguages` and `BattleNetInstallMetadata` (0BSD).

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use stoneform_app::InstallPlan;
use stoneform_app::ports::{DiskError, InstallRecords, InstalledBuild};
use stoneform_domain::{Locale, Product, Region};
use stoneform_tact::bpsv::Table;
use stoneform_tact::build_info::{BuildInfoRow, flavor_info, write_build_info};
use stoneform_tact::product_db::{ProductInstall, encode_install};

/// Writes the records into the install folder.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskInstallRecords;

impl InstallRecords for DiskInstallRecords {
    fn installed(&self, root: &Path, product: &Product) -> Result<Option<InstalledBuild>, DiskError> {
        let text = match fs::read_to_string(root.join(".build.info")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(DiskError::new(error)),
        };
        if !root.join(product.app_path).is_dir() {
            return Ok(None);
        }
        let table = Table::parse(&text);
        let installed = table
            .rows()
            .find(|row| row.get("Product") == Some(product.code) && row.get("Active") == Some("1"))
            .map(|row| InstalledBuild {
                version: row.get("Version").unwrap_or_default().to_owned(),
                build_key: row.get("Build Key").unwrap_or_default().to_owned(),
            });
        Ok(installed)
    }

    fn record_build(&self, root: &Path, plan: &InstallPlan, region: Region) -> Result<(), DiskError> {
        let path = root.join(".build.info");
        let existing = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(DiskError::new(error)),
        };
        let row = BuildInfoRow {
            region: region.as_str().to_owned(),
            build_key: plan.build.build_config.to_hex(),
            cdn_key: plan.build.cdn_config.to_hex(),
            cdn_path: plan.build.cdn.path.clone(),
            cdn_hosts: plan.build.cdn.hosts_field.clone(),
            cdn_servers: plan.build.cdn.servers_field.clone(),
            tags: plan.tags.clone(),
            version: plan.build.version.clone(),
            key_ring: plan.build.key_ring.clone(),
            product: plan.product.code.to_owned(),
        };
        write_atomically(&path, write_build_info(existing.as_deref(), &row).as_bytes()).map_err(DiskError::new)?;
        let flavor = root.join(flavor_folder(plan)).join(".flavor.info");
        write_atomically(&flavor, flavor_info(plan.product.code).as_bytes()).map_err(DiskError::new)
    }

    fn set_language(&self, flavor_root: &Path, locale: &Locale) -> Result<(), DiskError> {
        set_language(flavor_root, locale.as_str()).map_err(DiskError::new)
    }

    fn register_with_battle_net(
        &self,
        root: &Path,
        plan: &InstallPlan,
        region: Region,
        locale: &Locale,
    ) -> Result<(), DiskError> {
        // One `.product.db` per install root, as the Agent keeps it; an
        // existing one belongs to Battle.net or another flavor and stays.
        // The Agent's own database waits for milestone 3, which can check
        // that Battle.net isn't running before touching it.
        let path = root.join(".product.db");
        if path.exists() {
            return Ok(());
        }
        let install = ProductInstall {
            uid: plan.product.uid.to_owned(),
            product_code: plan.product.code.to_owned(),
            install_path: root.display().to_string(),
            region: Some(region.as_str().to_owned()),
            text_language: Some(locale.as_str().to_owned()),
            speech_language: Some(locale.as_str().to_owned()),
            version: Some(plan.build.version.clone()),
            build_config: Some(plan.build.build_config.to_hex()),
            tag_string: Some(plan.tags.clone()),
        };
        let record = encode_install(&install, flavor_folder(plan), "wow");
        write_atomically(&path, &record).map_err(DiskError::new)
    }
}

fn flavor_folder(plan: &InstallPlan) -> &str {
    if plan.product_config.subfolder.is_empty() { plan.product.flavor_folder } else { &plan.product_config.subfolder }
}

/// WoW reads its text and speech language from `WTF/Config.wtf`. On a fresh
/// install with one language for both, the launch locale is enough, so the
/// file is only edited when it exists: a language picked in game survives.
fn set_language(flavor_root: &Path, locale: &str) -> io::Result<()> {
    let path = flavor_root.join("WTF").join("Config.wtf");
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Latin-1 round-trips any bytes, for a file that isn't UTF-8.
    let (text, utf8) = match String::from_utf8(data) {
        Ok(text) => (text, true),
        Err(error) => (error.into_bytes().iter().map(|&byte| char::from(byte)).collect(), false),
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split(newline).map(str::to_owned).collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    for name in ["textLocale", "audioLocale"] {
        let line = format!("SET {name} \"{locale}\"");
        // CVar names are case-insensitive.
        let prefix = format!("set {} ", name.to_lowercase());
        match lines.iter_mut().find(|existing| existing.to_lowercase().starts_with(&prefix)) {
            Some(existing) => *existing = line,
            None => lines.push(line),
        }
    }
    let mut out = lines.join(newline);
    out.push_str(newline);
    let bytes: Vec<u8> = if utf8 { out.into_bytes() } else { out.chars().map(|c| c as u32 as u8).collect() };
    write_atomically(&path, &bytes)
}

fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("record path has no parent"))?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension("tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_config_wtf_alone_when_there_is_none() {
        let dir = tempfile::tempdir().unwrap();
        set_language(dir.path(), "deDE").unwrap();
        assert!(!dir.path().join("WTF").exists());
    }

    #[test]
    fn replaces_or_adds_the_language_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WTF").join("Config.wtf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "SET gxWindow \"1\"\r\nset TEXTLOCALE \"enUS\"\r\n").unwrap();

        set_language(dir.path(), "deDE").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "SET gxWindow \"1\"\r\nSET textLocale \"deDE\"\r\nSET audioLocale \"deDE\"\r\n"
        );
    }

    #[test]
    fn keeps_bytes_that_are_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WTF").join("Config.wtf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"SET accountName \"J\xf6rg\"\n").unwrap();

        set_language(dir.path(), "frFR").unwrap();

        let written = fs::read(&path).unwrap();
        assert!(written.starts_with(b"SET accountName \"J\xf6rg\"\n"));
        assert!(written.ends_with(b"SET audioLocale \"frFR\"\n"));
    }
}

#[cfg(test)]
mod installed_tests {
    use stoneform_domain::WOW_FOREVER;

    use super::*;

    const HEADER: &str = "Branch!STRING:0|Active!DEC:1|Build Key!HEX:16|Version!STRING:0|Product!STRING:0";

    fn root_with(build_info: &str, app: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".build.info"), build_info).unwrap();
        if app {
            fs::create_dir_all(dir.path().join(WOW_FOREVER.app_path)).unwrap();
        }
        dir
    }

    #[test]
    fn finds_the_active_forever_row() {
        let dir = root_with(&format!("{HEADER}\neu|1|abc|12.0.0.1|wow\neu|1|def|1.60.1.70888|wowf\n"), true);
        let installed = DiskInstallRecords.installed(dir.path(), &WOW_FOREVER).unwrap().unwrap();
        assert_eq!(installed, InstalledBuild { version: "1.60.1.70888".to_owned(), build_key: "def".to_owned() });
    }

    #[test]
    fn needs_both_the_row_and_the_app() {
        let other_flavor = root_with(&format!("{HEADER}\neu|1|abc|12.0.0.1|wow\n"), true);
        assert_eq!(DiskInstallRecords.installed(other_flavor.path(), &WOW_FOREVER).unwrap(), None);
        let no_app = root_with(&format!("{HEADER}\neu|1|def|1.60.1.70888|wowf\n"), false);
        assert_eq!(DiskInstallRecords.installed(no_app.path(), &WOW_FOREVER).unwrap(), None);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(DiskInstallRecords.installed(empty.path(), &WOW_FOREVER).unwrap(), None);
    }
}

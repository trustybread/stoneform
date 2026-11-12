//! TACT's text configs and the version service around them.
//!
//! A build config and a CDN config are `key = value value …` lines (`#`
//! comments). The build config names a build's manifests, the CDN config the
//! archives its files sit in. Which configs are live comes from the version
//! service's `versions` table, and which CDN serves them from its `cdns`
//! table (both [BPSV](crate::bpsv)). Only string building happens here; the
//! adapters do the fetching.

use std::collections::BTreeMap;

use crate::bpsv::{Row, Table};
use crate::{ContentKey, EncodingKey, Error, Result};

/// A parsed `key = v1 v2 …` config, keys in sorted order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    values: BTreeMap<String, Vec<String>>,
}

impl Config {
    /// Reads a config. Never fails: lines without `=` are skipped, and a key
    /// that appears twice keeps its last value.
    pub fn parse(text: &str) -> Self {
        let mut values = BTreeMap::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let Some((key, value)) = line.split_once('=') else { continue };
            let tokens = value.split(' ').filter(|token| !token.is_empty()).map(str::to_owned).collect();
            values.insert(key.trim().to_owned(), tokens);
        }
        Self { values }
    }

    /// The key's values; empty when the key is missing.
    pub fn get(&self, key: &str) -> &[String] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }

    pub fn first(&self, key: &str) -> Option<&str> {
        self.get(key).first().map(String::as_str)
    }

    /// Every key and its values, sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.values.iter().map(|(key, values)| (key.as_str(), values.as_slice()))
    }
}

/// A file the build config names that local storage keeps (the encoding
/// table, the manifests, VFS roots), stored with its full key in its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    /// The config key: `encoding`, `install`, `vfs-root`, …
    pub name: String,
    pub content_key: ContentKey,
    pub encoding_key: EncodingKey,
    /// The encoded size from `{name}-size`; 0 when the config doesn't say.
    pub encoded_size: u64,
}

/// A build config: which manifests make up a build.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildConfig(Config);

impl BuildConfig {
    pub fn parse(text: &str) -> Self {
        Self(Config::parse(text))
    }

    pub fn config(&self) -> &Config {
        &self.0
    }

    /// The content key of `name = <content key> [<encoding key>]`: its first value.
    pub fn content_key(&self, name: &str) -> Result<ContentKey> {
        let value = self.0.first(name).ok_or_else(|| Error::not_found(format!("`{name}` in the build config")))?;
        ContentKey::from_hex(value).map_err(|_| Error::malformed(format!("build config `{name}`")))
    }

    /// The encoding key of `name = <content key> <encoding key>`: its second value.
    pub fn encoding_key(&self, name: &str) -> Result<EncodingKey> {
        let value = self.0.get(name).get(1).ok_or_else(|| Error::not_found(format!("`{name}` in the build config")))?;
        EncodingKey::from_hex(value).map_err(|_| Error::malformed(format!("build config `{name}`")))
    }

    /// The root file's content key (look it up in the encoding table).
    pub fn root(&self) -> Result<ContentKey> {
        self.content_key("root")
    }

    pub fn encoding(&self) -> Result<EncodingKey> {
        self.encoding_key("encoding")
    }

    pub fn install(&self) -> Result<EncodingKey> {
        self.encoding_key("install")
    }

    pub fn download(&self) -> Result<EncodingKey> {
        self.encoding_key("download")
    }

    /// The encoded size of `name`: the last value of `{name}-size`.
    pub fn encoded_size(&self, name: &str) -> Option<u64> {
        self.0.get(&format!("{name}-size")).last()?.parse().ok()
    }

    /// The decoded size of `name`: the first value of `{name}-size`.
    pub fn content_size(&self, name: &str) -> Option<u64> {
        self.0.first(&format!("{name}-size"))?.parse().ok()
    }

    pub fn build_name(&self) -> Option<&str> {
        self.0.first("build-name")
    }

    /// The build-config files local storage holds with full keys: every
    /// `name = <content key> <encoding key>` except the size manifest, which
    /// isn't stored. Sorted by name. The root file isn't among them (it has
    /// only a content key and is stored with a truncated key, like game data).
    pub fn storage_files(&self) -> Vec<ConfigFile> {
        self.0
            .iter()
            .filter(|(name, values)| *name != "size" && values.len() == 2)
            .filter_map(|(name, values)| {
                Some(ConfigFile {
                    name: name.to_owned(),
                    content_key: ContentKey::from_hex(&values[0]).ok()?,
                    encoding_key: EncodingKey::from_hex(&values[1]).ok()?,
                    encoded_size: self.encoded_size(name).unwrap_or(0),
                })
            })
            .collect()
    }

    /// The patch manifest, `patch = <key>`, and its size from `patch-size`
    /// (0 when missing). Local storage keeps it raw (not BLTE) under that key;
    /// the CDN serves it from the patch path.
    pub fn patch(&self) -> Option<(EncodingKey, u64)> {
        let [value] = self.0.get("patch") else { return None };
        let key = EncodingKey::from_hex(value).ok()?;
        let size = self.0.first("patch-size").and_then(|size| size.parse().ok()).unwrap_or(0);
        Some((key, size))
    }
}

/// A CDN config: the archives a build's files sit in. Archive names are hex
/// keys; an archive's index is at its CDN data path plus `.index`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CdnConfig(Config);

impl CdnConfig {
    pub fn parse(text: &str) -> Self {
        Self(Config::parse(text))
    }

    pub fn config(&self) -> &Config {
        &self.0
    }

    /// Every data archive, in the order the archive group numbers them.
    pub fn archives(&self) -> &[String] {
        self.0.get("archives")
    }

    /// The name of the archive-group index over [`CdnConfig::archives`],
    /// which isn't on the CDN: see [`crate::archive_group`].
    pub fn archive_group(&self) -> Option<&str> {
        self.0.first("archive-group")
    }

    pub fn patch_archives(&self) -> &[String] {
        self.0.get("patch-archives")
    }

    pub fn patch_archive_group(&self) -> Option<&str> {
        self.0.first("patch-archive-group")
    }

    /// The index of files on the CDN outside any archive (keys and sizes).
    pub fn file_index(&self) -> Option<&str> {
        self.0.first("file-index")
    }

    pub fn patch_file_index(&self) -> Option<&str> {
        self.0.first("patch-file-index")
    }
}

/// The version service's `versions` or `cdns` table for a product, served
/// per region: `https://{region}.version.battle.net/v2/products/{product}/{file}`.
pub fn version_service_url(region: &str, product: &str, file: &str) -> String {
    format!("https://{}.version.battle.net/v2/products/{product}/{file}", region.to_ascii_lowercase())
}

/// One row of a product's `versions` table: the live build in a region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    pub region: String,
    /// Hex key of the build config (a CDN config file).
    pub build_config: String,
    pub cdn_config: String,
    pub build_id: u32,
    /// Human-readable version, such as `1.60.1.70862`.
    pub name: String,
    /// Hex key of the product config JSON (layout, binaries, tags).
    pub product_config: Option<String>,
    /// Key ring for encrypted content, when the product has one.
    pub key_ring: Option<String>,
}

/// The row of `table` (a `versions` table) for `region`, else the `us` row,
/// as the Battle.net Agent falls back.
pub fn select_version(table: &Table, region: &str) -> Result<VersionRow> {
    let row =
        region_row(table, "Region", region).ok_or_else(|| Error::not_found(format!("a version for {region} or us")))?;
    let required = |column: &str| {
        row.non_empty(column)
            .map(str::to_owned)
            .ok_or_else(|| Error::malformed(format!("versions table: no {column} for {region}")))
    };
    Ok(VersionRow {
        region: row.non_empty("Region").unwrap_or(region).to_owned(),
        build_config: required("BuildConfig")?,
        cdn_config: required("CDNConfig")?,
        build_id: row.get("BuildId").and_then(|id| id.parse().ok()).unwrap_or(0),
        name: required("VersionsName")?,
        product_config: row.non_empty("ProductConfig").map(str::to_owned),
        key_ring: row.non_empty("KeyRing").map(str::to_owned),
    })
}

/// One row of a product's `cdns` table: where a region's CDN files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdnRow {
    pub name: String,
    /// The product's folder on every mirror, such as `tpr/wow`.
    pub path: String,
    /// Where product configs live, such as `tpr/configs/data`.
    pub config_path: Option<String>,
    /// The `Hosts` column split on spaces, for `.build.info`.
    pub hosts: Vec<String>,
    /// The `Servers` column verbatim, for `.build.info`.
    pub servers_field: String,
    /// Mirrors to fetch from, as `https://host[:port]` without a trailing slash.
    pub servers: Vec<String>,
}

/// The row of `table` (a `cdns` table) for `region`, else the `us` row.
///
/// Only HTTPS mirrors are used: the `Servers` column's `https://` entries,
/// with their path and query (`/?maxhosts=4`) dropped; when it has none,
/// `https://{host}` for every host in `Hosts`. Fails when that leaves none.
pub fn select_cdn(table: &Table, region: &str) -> Result<CdnRow> {
    let row = region_row(table, "Name", region).ok_or_else(|| Error::not_found(format!("a CDN for {region} or us")))?;
    let path = row.non_empty("Path").ok_or_else(|| Error::malformed(format!("cdns table: no Path for {region}")))?;
    let servers_field = row.get("Servers").unwrap_or_default();
    let hosts: Vec<String> =
        row.get("Hosts").unwrap_or_default().split(' ').filter(|h| !h.is_empty()).map(str::to_owned).collect();
    let mut servers: Vec<String> = servers_field.split(' ').filter_map(https_origin).collect();
    if servers.is_empty() {
        servers = hosts.iter().map(|host| format!("https://{host}")).collect();
    }
    if servers.is_empty() {
        return Err(Error::not_found(format!("an HTTPS CDN mirror for {region}")));
    }
    Ok(CdnRow {
        name: row.non_empty("Name").unwrap_or(region).to_owned(),
        path: path.to_owned(),
        config_path: row.non_empty("ConfigPath").map(str::to_owned),
        hosts,
        servers_field: servers_field.to_owned(),
        servers,
    })
}

fn region_row<'a>(table: &'a Table, column: &str, region: &str) -> Option<Row<'a>> {
    let matching =
        |wanted: &str| table.rows().find(|row| row.get(column).is_some_and(|r| r.eq_ignore_ascii_case(wanted)));
    matching(region).or_else(|| matching("us"))
}

/// `https://host[:port]` from an `https://` URL; `None` for any other scheme
/// or an empty host.
fn https_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    (!authority.is_empty()).then(|| format!("https://{authority}"))
}

/// Which tree of a product's CDN folder a file is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdnKind {
    /// Build and CDN configs.
    Config,
    /// Archives, archive indexes (`.index`), and BLTE files outside archives.
    Data,
    /// Patch manifests and patch archives.
    Patch,
}

impl CdnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CdnKind::Config => "config",
            CdnKind::Data => "data",
            CdnKind::Patch => "patch",
        }
    }
}

/// A file's path on every CDN mirror: `{path}/{kind}/{h[0:2]}/{h[2:4]}/{h}`,
/// with the hash in lowercase. Append `.index` for an archive's index.
pub fn cdn_path(path: &str, kind: CdnKind, hash: &str) -> String {
    hashed_path(&format!("{path}/{}", kind.as_str()), hash)
}

/// A product config's path: directly under the `cdns` table's `ConfigPath`,
/// `{config_path}/{h[0:2]}/{h[2:4]}/{h}`.
pub fn product_config_path(config_path: &str, hash: &str) -> String {
    hashed_path(config_path, hash)
}

fn hashed_path(folder: &str, hash: &str) -> String {
    let hash = hash.to_ascii_lowercase();
    let first: String = hash.chars().take(2).collect();
    let second: String = hash.chars().skip(2).take(2).collect();
    format!("{folder}/{first}/{second}/{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODING: &str = "00112233445566778899aabbccddeeff";

    /// The config half of Waypoint's `parsesVersionTablesAndConfigs`.
    #[test]
    fn parses_configs() {
        let config = Config::parse("# Build Configuration\n\ninstall = 0011 2233\nbuild-name = 253932_36.6.3\n");
        assert_eq!(config.get("install"), ["0011", "2233"]);
        assert_eq!(config.get("build-name"), ["253932_36.6.3"]);
        assert!(config.get("missing").is_empty());
    }

    #[test]
    fn reads_typed_build_config_values() {
        let content = "ffeeddccbbaa99887766554433221100";
        let text = format!(
            "# Build Configuration\r\nroot = {content}\r\ninstall = {content} {ENCODING}\r\ninstall-size = 1000 900\r\n\
             encoding = {content} {ENCODING}\r\nencoding-size = 5000 4000\r\nsize = {content} {ENCODING}\r\n\
             vfs-root = {content} {ENCODING}\r\npatch = {ENCODING}\r\npatch-size = 777\r\nbuild-name = WOW-70862patch1.60.1\r\n"
        );
        let build = BuildConfig::parse(&text);
        assert_eq!(build.root().unwrap().to_hex(), content);
        assert_eq!(build.install().unwrap().to_hex(), ENCODING);
        assert_eq!(build.encoding().unwrap().to_hex(), ENCODING);
        assert!(matches!(build.download(), Err(Error::NotFound(_))));
        assert_eq!(build.encoded_size("install"), Some(900));
        assert_eq!(build.content_size("install"), Some(1000));
        assert_eq!(build.build_name(), Some("WOW-70862patch1.60.1"));
        let files: Vec<(String, u64)> = build.storage_files().into_iter().map(|f| (f.name, f.encoded_size)).collect();
        assert_eq!(files, [("encoding".into(), 4000), ("install".into(), 900), ("vfs-root".into(), 0)]);
        assert_eq!(build.patch(), Some((EncodingKey::from_hex(ENCODING).unwrap(), 777)));
        assert!(matches!(BuildConfig::parse("install = 00 zz").install(), Err(Error::Malformed(_))));
    }

    #[test]
    fn reads_cdn_config_lists() {
        let cdn = CdnConfig::parse("archives = aa bb cc\narchive-group = dd\npatch-archives = ee\nfile-index = ff\n");
        assert_eq!(cdn.archives(), ["aa", "bb", "cc"]);
        assert_eq!(cdn.archive_group(), Some("dd"));
        assert_eq!(cdn.patch_archives(), ["ee"]);
        assert_eq!(cdn.patch_archive_group(), None);
        assert_eq!(cdn.file_index(), Some("ff"));
    }

    #[test]
    fn selects_the_region_row_and_falls_back_to_us() {
        let table = Table::parse(
            "Region!STRING:0|BuildConfig!HEX:16|CDNConfig!HEX:16|KeyRing!HEX:16|BuildId!DEC:4|VersionsName!String:0|ProductConfig!HEX:16\n\
             ## seqn = 1\n\
             us|b1|c1||70862|1.60.1.70862|p1\n\
             eu|b2|c2|k2|70863|1.60.1.70863|\n",
        );
        let eu = select_version(&table, "eu").unwrap();
        assert_eq!((eu.build_config.as_str(), eu.build_id, eu.key_ring.as_deref()), ("b2", 70863, Some("k2")));
        assert_eq!(eu.product_config, None);
        let kr = select_version(&table, "kr").unwrap();
        assert_eq!(
            (kr.region.as_str(), kr.name.as_str(), kr.product_config.as_deref()),
            ("us", "1.60.1.70862", Some("p1"))
        );
        assert!(select_version(&Table::parse("Region!STRING:0\neu\n"), "kr").is_err());
        assert!(select_version(&Table::parse("Region!STRING:0\neu\n"), "eu").is_err());
    }

    #[test]
    fn keeps_https_mirrors_without_their_path() {
        let table = Table::parse(
            "Name!STRING:0|Path!STRING:0|Hosts!STRING:0|Servers!STRING:0|ConfigPath!STRING:0\n\
             eu|tpr/wow|eu.cdn.blizzard.com level3.blizzard.com|http://eu.cdn.blizzard.com/?maxhosts=4 https://eu.cdn.blizzard.com/?maxhosts=4 https://blzddist1-a.akamaihd.net:443/x|tpr/configs/data\n\
             us|tpr/wow|us.cdn.blizzard.com||\n",
        );
        let eu = select_cdn(&table, "eu").unwrap();
        assert_eq!(eu.servers, ["https://eu.cdn.blizzard.com", "https://blzddist1-a.akamaihd.net:443"]);
        assert_eq!(eu.hosts, ["eu.cdn.blizzard.com", "level3.blizzard.com"]);
        assert!(eu.servers_field.starts_with("http://eu.cdn.blizzard.com/?maxhosts=4 "));
        assert_eq!(eu.config_path.as_deref(), Some("tpr/configs/data"));
        // No HTTPS server: every host over HTTPS. Unknown region: us.
        let us = select_cdn(&table, "kr").unwrap();
        assert_eq!(us.servers, ["https://us.cdn.blizzard.com"]);
        assert_eq!(us.config_path, None);
        assert!(select_cdn(&Table::parse("Name!STRING:0|Path!STRING:0|Hosts!STRING:0\nus|tpr/wow|\n"), "us").is_err());
    }

    #[test]
    fn builds_cdn_paths() {
        assert_eq!(cdn_path("tpr/wow", CdnKind::Config, "ABCDEF0123"), "tpr/wow/config/ab/cd/abcdef0123");
        assert_eq!(
            cdn_path("tpr/wow", CdnKind::Data, ENCODING) + ".index",
            format!("tpr/wow/data/00/11/{ENCODING}.index")
        );
        assert_eq!(cdn_path("tpr/wow", CdnKind::Patch, "a"), "tpr/wow/patch/a//a");
        assert_eq!(product_config_path("tpr/configs/data", "0d21f2f2"), "tpr/configs/data/0d/21/0d21f2f2");
        assert_eq!(
            version_service_url("EU", "wowf", "versions"),
            "https://eu.version.battle.net/v2/products/wowf/versions"
        );
    }
}

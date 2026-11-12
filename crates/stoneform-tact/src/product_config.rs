//! The parts of a product's Blizzard "product config" that say how a game is
//! laid out on macOS: the JSON on the CDN that the `versions` table's
//! `ProductConfig` column names. It's what the Battle.net Agent itself
//! follows, so installs made from it match the Agent's layout.

use serde_json::{Map, Value};

use crate::languages::Languages;
use crate::{Error, Result};

/// A product config, read for macOS (`platform.mac.config`, over `all.config`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductConfig {
    /// Whether the config has a `platform.mac` section at all.
    pub has_platform_configuration: bool,
    /// Where loose (install manifest) files go, relative to the install
    /// root: a flavor folder such as `_forever_`. Empty: the root.
    pub subfolder: String,
    /// The local storage folder, such as `Data/`.
    pub data_directory: String,
    pub supported_locales: Vec<String>,
    /// Tags for a full macOS install: the platform (`OSX`) and, for some
    /// games, content sets or `code`.
    pub platform_tags: Vec<String>,
    /// Tags the Agent adds: CPU architectures (WoW), `Alternate`, `noigr`.
    pub extra_tags: Vec<String>,
    /// The game's `.app`, relative to the subfolder.
    pub game_binary: Option<String>,
    pub launch_arguments: Vec<String>,
    pub sso_launch_argument: Option<String>,
    /// Default folder name under /Applications, when the config names one.
    pub folder_name: Option<String>,
    /// Stored as loose files only, with no local storage (Hearthstone).
    pub is_containerless: bool,
}

impl ProductConfig {
    pub fn parse(json: &[u8]) -> Result<Self> {
        let root: Value =
            serde_json::from_slice(json).map_err(|error| Error::malformed(format!("product config ({error})")))?;
        let root = root.as_object().ok_or_else(|| Error::malformed("product config (not an object)"))?;
        let empty = Map::new();
        let all = object(root.get("all"), &["config"]).unwrap_or(&empty);
        let mac = object(root.get("platform"), &["mac", "config"]);
        let config = mac.unwrap_or(&empty);

        let mut extra_tags = Vec::new();
        if let Some(bits64) = strings(config.get("tags_64bit")) {
            // The Agent picks the 64-bit set; WoW's manifests also carry
            // arm64 files for Apple silicon, so take both.
            extra_tags.extend(bits64);
            extra_tags.push("arm64".to_owned());
        }
        extra_tags.extend(strings(all.get("extra_tags")).unwrap_or_default());
        extra_tags.extend(strings(all.get("noigr_tags")).unwrap_or_default());

        let game = object(config.get("binaries"), &["game"]).unwrap_or(&empty);
        let launch_arguments = strings(game.get("launch_arguments"))
            .or_else(|| strings(config.get("launch_arguments")))
            .or_else(|| strings(all.get("launch_arguments")))
            .unwrap_or_default();
        let either = |path: &[&str]| string_at(config, path).or_else(|| string_at(all, path));
        let containerless = |section: &Map<String, Value>| {
            string_at(section, &["update_method"]).is_some_and(|method| method.contains("containerless"))
        };

        Ok(Self {
            has_platform_configuration: mac.is_some(),
            subfolder: string_at(all, &["shared_container_default_subfolder"]).unwrap_or_default(),
            data_directory: string_at(all, &["data_dir"]).unwrap_or_else(|| "Data/".to_owned()),
            supported_locales: strings(all.get("supported_locales")).unwrap_or_default(),
            platform_tags: strings(config.get("tags")).unwrap_or_else(|| vec!["OSX".to_owned()]),
            extra_tags,
            game_binary: string_at(game, &["relative_path_64"]).or_else(|| string_at(game, &["relative_path"])),
            launch_arguments,
            sso_launch_argument: either(&["opaque_product_specific", "sso_launch_argument"]),
            folder_name: either(&["form", "game_dir", "dirname"]),
            is_containerless: containerless(config) || containerless(all),
        })
    }

    /// The tag string the Agent records for an install: a speech set in the
    /// spoken language and a text set in the text language, each with the
    /// platform and extra tags and the region (`EU?`).
    pub fn tag_string(&self, region: &str, languages: &Languages) -> String {
        let mut common: Vec<String> = self.platform_tags.iter().chain(&self.extra_tags).cloned().collect();
        common.push(format!("{}?", region.to_ascii_uppercase()));
        let common = common.join(" ");
        format!("{common} {} speech?:{common} {} text?", languages.speech, languages.text)
    }

    /// The arguments to start the game with: the config's, with the sign-in
    /// argument first. `catalog_sso` is the catalog's `sso_launch_argument`
    /// for the install, which wins over the config's (even when empty).
    pub fn launch_arguments_with_sso(&self, catalog_sso: Option<&str>) -> Vec<String> {
        let mut arguments = self.launch_arguments.clone();
        let sso = catalog_sso.or(self.sso_launch_argument.as_deref()).filter(|sso| !sso.is_empty());
        if let Some(sso) = sso {
            if !arguments.iter().any(|argument| argument == sso) {
                arguments.insert(0, sso.to_owned());
            }
        }
        arguments
    }
}

/// The object at `path` below `value`.
fn object<'a>(value: Option<&'a Value>, path: &[&str]) -> Option<&'a Map<String, Value>> {
    path.iter().try_fold(value?.as_object()?, |object, key| object.get(*key)?.as_object())
}

fn string_at(object: &Map<String, Value>, path: &[&str]) -> Option<String> {
    let (last, parents) = path.split_last()?;
    let parent = parents.iter().try_fold(object, |object, key| object.get(*key)?.as_object())?;
    parent.get(*last)?.as_str().map(str::to_owned)
}

/// An array of strings; `None` when missing or anything else (an array with
/// a non-string in it too, as the Agent's typed reads go).
fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_array()?.iter().map(|item| item.as_str().map(str::to_owned)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WOWF: &[u8] = include_bytes!("../tests/fixtures/wowf-product.json");

    /// WoW: Forever's product config as the CDN served it on 2026-11-04
    /// (Waypoint's catalog fixture).
    #[test]
    fn reads_wow_forever() {
        let config = ProductConfig::parse(WOWF).unwrap();
        assert!(config.has_platform_configuration);
        assert_eq!(config.subfolder, "_forever_");
        assert_eq!(config.data_directory, "Data/");
        assert_eq!(config.folder_name.as_deref(), Some("World of Warcraft"));
        assert_eq!(config.game_binary.as_deref(), Some("World of Warcraft.app"));
        assert!(config.supported_locales.iter().any(|locale| locale == "ruRU"));
        assert_eq!(config.platform_tags, ["OSX"]);
        assert_eq!(config.extra_tags, ["x86_64", "arm64"]);
        assert!(config.launch_arguments.is_empty());
        assert_eq!(config.sso_launch_argument, None);
        assert!(!config.is_containerless);
        // The catalog's `-launcherlogin` makes the launch arguments.
        assert_eq!(config.launch_arguments_with_sso(Some("-launcherlogin")), ["-launcherlogin"]);
        assert_eq!(
            config.tag_string("eu", &Languages::new("enUS", None)),
            "OSX x86_64 arm64 EU? enUS speech?:OSX x86_64 arm64 EU? enUS text?"
        );
    }

    /// Port of Waypoint's `buildsTagsForTwoLanguagesLikeTheAgent`.
    #[test]
    fn builds_tags_for_two_languages_like_the_agent() {
        let config = ProductConfig::parse(
            br#"{"all":{"config":{"data_dir":"Data/"}},"platform":{"mac":{"config":{"tags":["OSX"],"tags_64bit":["x86_64"]}}}}"#,
        )
        .unwrap();
        assert_eq!(
            config.tag_string("eu", &Languages::new("ruRU", Some("enUS"))),
            "OSX x86_64 arm64 EU? enUS speech?:OSX x86_64 arm64 EU? ruRU text?"
        );
    }

    /// Port of Waypoint's `readsProductConfigsLikeTheAgent`.
    #[test]
    fn reads_product_configs_like_the_agent() {
        let hearthstone = ProductConfig::parse(
            br#"{"all":{"config":{"data_dir":"Data/","supported_locales":["enUS","ruRU"]}},
                "platform":{"mac":{"config":{"update_method":"containerless ngdp","tags":["OSX","manifest","base"],
                "binaries":{"game":{"relative_path":"Hearthstone.app","launch_arguments":["-launch"]}}}}}}"#,
        )
        .unwrap();
        assert!(hearthstone.is_containerless);
        assert_eq!(hearthstone.subfolder, "");
        assert_eq!(
            hearthstone.tag_string("eu", &Languages::new("ruRU", None)),
            "OSX manifest base EU? ruRU speech?:OSX manifest base EU? ruRU text?"
        );
        assert_eq!(hearthstone.launch_arguments, ["-launch"]);

        let wow = ProductConfig::parse(
            br#"{"all":{"config":{"data_dir":"Data/","shared_container_default_subfolder":"_classic_era_"}},
                "platform":{"mac":{"config":{"tags":["OSX"],"tags_64bit":["x86_64"],
                "binaries":{"game":{"relative_path":"World of Warcraft Classic.app","launch_arguments":[]}}}}}}"#,
        )
        .unwrap();
        assert!(!wow.is_containerless);
        assert_eq!(wow.subfolder, "_classic_era_");
        assert!(wow.tag_string("us", &Languages::new("enUS", None)).starts_with("OSX x86_64 arm64 US? enUS speech?"));

        let starcraft = ProductConfig::parse(
            br#"{"all":{"config":{"data_dir":"Data/","noigr_tags":["noigr"]}},
                "platform":{"mac":{"config":{"tags":["OSX"],"binaries":{"game":{"relative_path":"x86/StarCraft.app",
                "relative_path_64":"x86_64/StarCraft.app","launch_arguments":["-launch"]}}}}}}"#,
        )
        .unwrap();
        assert_eq!(starcraft.game_binary.as_deref(), Some("x86_64/StarCraft.app"));
        assert_eq!(starcraft.extra_tags, ["noigr"]);
    }

    /// Port of Waypoint's `discoveredProductsKeepTheProductConfigsSSOArgument`.
    #[test]
    fn puts_the_sso_argument_first() {
        let config = ProductConfig::parse(
            br#"{"all":{"config":{"form":{"game_dir":{"dirname":"StarCraft II"}},"supported_locales":["enUS"],
                "opaque_product_specific":{"sso_launch_argument":"-sso=1"}}},
                "platform":{"mac":{"config":{"binaries":{"game":{"relative_path":"Support/SC2Switcher.app","launch_arguments":["-launch"]}}}}}}"#,
        )
        .unwrap();
        assert_eq!(config.folder_name.as_deref(), Some("StarCraft II"));
        assert_eq!(config.launch_arguments_with_sso(None), ["-sso=1", "-launch"]);
        assert_eq!(config.launch_arguments_with_sso(Some("-sso=1")), ["-sso=1", "-launch"]);
        // As in Waypoint, an empty catalog argument still wins, and means none.
        assert_eq!(config.launch_arguments_with_sso(Some("")), ["-launch"]);
    }

    #[test]
    fn falls_back_without_a_mac_section() {
        let config = ProductConfig::parse(
            br#"{"all":{"config":{"supported_locales":["enUS"]}},"platform":{"win":{"config":{}}}}"#,
        )
        .unwrap();
        assert!(!config.has_platform_configuration);
        assert_eq!(config.platform_tags, ["OSX"]);
        assert!(config.extra_tags.is_empty());
        assert_eq!(ProductConfig::parse(b"{}").unwrap().data_directory, "Data/");
        assert!(matches!(ProductConfig::parse(b"[]"), Err(Error::Malformed(_))));
        assert!(matches!(ProductConfig::parse(b"{"), Err(Error::Malformed(_))));
    }
}

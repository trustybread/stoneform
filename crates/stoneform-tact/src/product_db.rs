//! Battle.net's install records: the Agent-wide `product.db` (a database of
//! installs) and the `.product.db` in every game folder (one install, or a
//! database when a WoW root holds several flavors).
//!
//! Field numbers, as Waypoint took them from the files on disk:
//!
//! ```text
//! Database           { repeated ProductInstall installs = 1; }
//! ProductInstall     { uid = 1; product_code = 2; UserSettings settings = 3;
//!                      CachedProductState cached_product_state = 4; family = 6; }
//! UserSettings       { install_path = 1; play_region = 2; selected_text_language = 6;
//!                      selected_speech_language = 7; languages = 8; additional_tags = 9;
//!                      subfolder = 13; }
//! CachedProductState { BaseProductState base_product_state = 1; }
//! BaseProductState   { current_version_str = 7; completed_build_keys = 12;
//!                      active_build_key = 14; active_tag_string = 17; }
//! ```

use crate::protobuf::{Fields, Reader, Value, Writer};
use crate::{Result, tags};

/// Products that are part of Battle.net itself, not games.
pub const INFRASTRUCTURE_CODES: [&str; 2] = ["agent", "bna"];

/// One installed product as the Agent records it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProductInstall {
    /// The install uid (`wowf`): what Battle.net passes to games as `-uid`.
    pub uid: String,
    /// The TACT product code (`wowf`).
    pub product_code: String,
    pub install_path: String,
    /// `eu`, `us`, `kr`, `cn`.
    pub region: Option<String>,
    pub text_language: Option<String>,
    pub speech_language: Option<String>,
    pub version: Option<String>,
    /// The installed build config's hex key.
    pub build_config: Option<String>,
    /// The tags the install selected, such as `OSX … EU? enUS speech?:… enUS text?`.
    pub tag_string: Option<String>,
}

/// The installs in an Agent database, without Battle.net's own.
pub fn parse_database(data: &[u8]) -> Result<Vec<ProductInstall>> {
    let fields = Reader::new(data).fields()?;
    let mut installs = Vec::new();
    for value in fields.get(&1).into_iter().flatten() {
        let Some(message) = value.as_bytes() else { continue };
        if let Some(install) = parse_install(message)? {
            if !INFRASTRUCTURE_CODES.contains(&install.product_code.as_str()) {
                installs.push(install);
            }
        }
    }
    Ok(installs)
}

/// One ProductInstall record, without a database around it. `None` when it
/// lacks a uid, product code or install path.
pub fn parse_install(data: &[u8]) -> Result<Option<ProductInstall>> {
    let fields = Reader::new(data).fields()?;
    let (Some(uid), Some(product_code)) = (string(&fields, 1), string(&fields, 2)) else { return Ok(None) };
    let Some(settings) = message(&fields, 3)? else { return Ok(None) };
    let Some(install_path) = string(&settings, 1).filter(|path| !path.is_empty()) else { return Ok(None) };
    let state = match message(&fields, 4)? {
        Some(cached) => message(&cached, 1)?.unwrap_or_default(),
        None => Fields::new(),
    };
    let non_empty = |fields: &Fields<'_>, number| string(fields, number).filter(|value| !value.is_empty());
    let last_build = state.get(&12).and_then(|values| values.last()).and_then(Value::as_str).filter(|v| !v.is_empty());
    Ok(Some(ProductInstall {
        uid,
        product_code,
        install_path,
        region: non_empty(&settings, 2),
        text_language: non_empty(&settings, 6),
        speech_language: non_empty(&settings, 7),
        version: non_empty(&state, 7),
        build_config: non_empty(&state, 14).or_else(|| last_build.map(str::to_owned)),
        tag_string: non_empty(&state, 17),
    }))
}

/// A game folder's `.product.db`: the Agent writes either one record or a
/// database (shared WoW roots can list several flavors). A database is
/// preferred whenever one can be read, since its unknown fields can look
/// like a single record's.
pub fn parse_folder_installs(data: &[u8]) -> Result<Vec<ProductInstall>> {
    if let Ok(installs) = parse_database(data) {
        if !installs.is_empty() {
            return Ok(installs);
        }
    }
    Ok(parse_install(data)?.into_iter().collect())
}

/// The record the Agent writes for a finished macOS install (a folder's
/// `.product.db`; wrap it as field 1 to add it to an Agent database).
///
/// `subfolder` is the flavor folder (`_forever_`). `family` is the Agent's
/// product family: `wow` for every WoW flavor, else the product code. A WoW
/// install whose tags select HighRes in every set also records it in
/// `additional_tags`: without the package there, the Agent would want to
/// download it. Missing values get the defaults of Agent-created installs.
pub fn encode_install(install: &ProductInstall, subfolder: &str, family: &str) -> Vec<u8> {
    let text = install.text_language.as_deref().unwrap_or("enUS");
    let speech = install.speech_language.as_deref().unwrap_or(text);
    let build = install.build_config.as_deref().unwrap_or_default();
    let high_res = family == "wow" && install.tag_string.as_deref().is_some_and(tags::selects_high_res);
    let mut languages = vec![text, speech];
    languages.sort_unstable();
    languages.dedup();

    let mut w = Writer::new();
    w.string(1, &install.uid);
    w.string(2, &install.product_code);
    w.message(3, |settings| {
        settings.string(1, &install.install_path);
        settings.string(2, install.region.as_deref().unwrap_or("us"));
        settings.varint(3, 2);
        settings.varint(4, 2);
        settings.varint(5, 3);
        settings.string(6, text);
        settings.string(7, speech);
        if high_res {
            settings.string(9, tags::HIGH_RES);
        }
        for language in &languages {
            settings.message(8, |entry| {
                entry.string(1, language);
                // Flags: 2 = the text language, 1 = the spoken one.
                entry.varint(2, u64::from(*language == text) << 1 | u64::from(*language == speech));
            });
        }
        settings.string(13, subfolder);
    });
    w.message(4, |cached| {
        cached.message(1, |state| {
            state.varint(1, 1);
            state.varint(2, 1);
            state.varint(3, 1);
            state.varint(4, 0);
            state.varint(5, 0);
            state.string(7, install.version.as_deref().unwrap_or_default());
            state.string(10, "");
            state.string(12, build);
            state.string(14, build);
            state.string(17, install.tag_string.as_deref().unwrap_or("OSX"));
            state.varint(21, 0);
        });
        cached.message(2, |backfill| {
            backfill.fixed64(1, 0);
            backfill.varint(2, 0);
            backfill.varint(3, 0);
            backfill.varint(5, 0);
        });
        cached.message(3, |progress| progress.fixed64(1, 0));
        cached.message(4, |progress| {
            progress.fixed64(2, 1f64.to_bits());
            progress.varint(4, 0);
            progress.varint(5, 0);
            progress.message(6, |build_progress| build_progress.string(1, build));
        });
    });
    w.string(6, family);
    w.varint(7, 0);
    w.string(8, r#"{"user_install_package_settings":[]}"#);
    w.into_bytes()
}

fn string(fields: &Fields<'_>, number: u32) -> Option<String> {
    fields.get(&number)?.first()?.as_str().map(str::to_owned)
}

fn message<'a>(fields: &Fields<'a>, number: u32) -> Result<Option<Fields<'a>>> {
    match fields.get(&number).and_then(|values| values.first()).and_then(Value::as_message) {
        Some(reader) => reader.fields().map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(w: &mut Writer, uid: &str, code: &str, path: &str, region: &str, version: &str) {
        w.message(1, |p| {
            p.string(1, uid);
            p.string(2, code);
            p.message(3, |s| {
                s.string(1, path);
                s.string(2, region);
                s.varint(3, 2);
                s.string(6, "enUS");
                s.string(7, "deDE");
            });
            p.message(4, |c| {
                c.message(1, |b| {
                    b.varint(1, 1);
                    b.string(7, version);
                })
            });
        });
    }

    /// Port of Waypoint's `parsesAgentDatabaseAndSkipsInfrastructure`.
    #[test]
    fn parses_agent_database_and_skips_infrastructure() {
        let mut w = Writer::new();
        install(&mut w, "agent", "agent", "/Users/Shared/Battle.net/Agent", "us", "2.41.1.9824");
        install(&mut w, "battle.net", "bna", "/Applications/Battle.net.app", "us", "2.53.4.17896");
        install(&mut w, "hs_beta", "hsb", "/Applications/Hearthstone", "eu", "36.6.3.253932.253216");
        install(&mut w, "wow", "wow", "/Applications/World of Warcraft", "eu", "12.0.1.66000");
        w.varint(6, 4057155);
        w.string(7, "hs_beta");

        let installs = parse_database(&w.into_bytes()).unwrap();
        assert_eq!(installs.iter().map(|i| i.uid.as_str()).collect::<Vec<_>>(), ["hs_beta", "wow"]);
        assert_eq!(
            installs[0],
            ProductInstall {
                uid: "hs_beta".into(),
                product_code: "hsb".into(),
                install_path: "/Applications/Hearthstone".into(),
                region: Some("eu".into()),
                text_language: Some("enUS".into()),
                speech_language: Some("deDE".into()),
                version: Some("36.6.3.253932.253216".into()),
                ..Default::default()
            }
        );
    }

    /// Port of Waypoint's `parsesPerInstallFile`.
    #[test]
    fn parses_per_install_file() {
        let mut w = Writer::new();
        w.string(1, "hs_beta");
        w.string(2, "hsb");
        w.message(3, |s| {
            s.string(1, "/Applications/Hearthstone");
            s.string(2, "eu");
        });
        let install = parse_install(&w.into_bytes()).unwrap().unwrap();
        assert_eq!(install.uid, "hs_beta");
        assert_eq!(install.region.as_deref(), Some("eu"));
        assert_eq!(install.version, None);
    }

    /// Port of Waypoint's `parsesFolderDatabaseDespiteUnknownSingleRecordShapedFields`.
    #[test]
    fn parses_folder_database_despite_unknown_single_record_shaped_fields() {
        for valid_unknown_settings in [false, true] {
            let mut w = Writer::new();
            install(&mut w, "wow", "wow", "/Games/WoW", "eu", "1.0");
            w.string(2, "unknown database field");
            if valid_unknown_settings {
                w.message(3, |s| s.string(1, "/Games/other"));
            } else {
                w.bytes(3, &[0xff]);
            }
            let installs = parse_folder_installs(&w.into_bytes()).unwrap();
            assert_eq!(installs.iter().map(|i| i.uid.as_str()).collect::<Vec<_>>(), ["wow"]);
        }
    }

    /// Ports of Waypoint's `rejectsTruncatedData` and
    /// `acceptsMaximumProtobufValuesAndExactPayloadBounds`.
    #[test]
    fn rejects_truncated_data_and_accepts_exact_bounds() {
        assert!(parse_database(&[0x0a, 0x10, 0x01]).is_err());
        let mut w = Writer::new();
        install(&mut w, "wow", "wow", "/Games/World of Warcraft", "eu", "12.0.1");
        w.varint(99, u64::MAX);
        w.fixed64(100, u64::MAX);
        w.bytes(101, &[]);
        let data = w.into_bytes();
        assert_eq!(Reader::new(&data).fields().unwrap()[&99][0].as_varint(), Some(u64::MAX));
        assert_eq!(parse_database(&data).unwrap().len(), 1);
        assert!(parse_database(&[0x0a, 0x00]).unwrap().is_empty());
    }

    fn wowf() -> ProductInstall {
        ProductInstall {
            uid: "wowf".into(),
            product_code: "wowf".into(),
            install_path: "/Applications/World of Warcraft".into(),
            region: Some("eu".into()),
            text_language: Some("ruRU".into()),
            speech_language: Some("enUS".into()),
            version: Some("12.1.5.70783".into()),
            build_config: Some("a".repeat(32)),
            tag_string: Some("OSX x86_64 arm64 EU? ruRU text?:OSX x86_64 arm64 EU? enUS speech?".into()),
        }
    }

    /// Port of Waypoint's `metadataEncodesSeparateLanguageFlagsAndSharedSubfolder`.
    #[test]
    fn encodes_separate_language_flags_and_shared_subfolder() {
        let data = encode_install(&wowf(), "_forever_", "wow");
        let fields = Reader::new(&data).fields().unwrap();
        let settings = message(&fields, 3).unwrap().unwrap();
        assert_eq!(string(&settings, 13).as_deref(), Some("_forever_"));
        let languages: Vec<Fields<'_>> =
            settings[&8].iter().map(|value| value.as_message().unwrap().fields().unwrap()).collect();
        assert_eq!(languages.len(), 2);
        let flag = |locale: &str| {
            languages.iter().find(|l| string(l, 1).as_deref() == Some(locale)).and_then(|l| l[&2][0].as_varint())
        };
        assert_eq!((flag("ruRU"), flag("enUS")), (Some(2), Some(1)));
        assert_eq!(string(&fields, 6).as_deref(), Some("wow"));
        // No HighRes without it in every set.
        assert!(!settings.contains_key(&9));
        // What it writes reads back.
        let parsed = parse_install(&data).unwrap().unwrap();
        assert_eq!(parsed, wowf());
        assert_eq!(parse_folder_installs(&data).unwrap(), [wowf()]);
    }

    /// The encode lines of Waypoint's `highResMetadataPreservesUnknownFieldsAndOtherEditions`
    /// and `metadataLeavesContentSelectionOfInstallsWithoutHighResAlone`.
    #[test]
    fn records_high_res_only_when_every_set_selects_it() {
        let mut completed = wowf();
        completed.tag_string = completed.tag_string.as_deref().map(tags::with_high_res);
        let data = encode_install(&completed, "_forever_", "wow");
        let settings = message(&Reader::new(&data).fields().unwrap(), 3).unwrap().unwrap();
        assert_eq!(string(&settings, 9).as_deref(), Some("HighRes"));

        let mut partial = wowf();
        partial.tag_string = Some("OSX x86_64 arm64 EU? HighRes ruRU text?:OSX x86_64 arm64 EU? enUS speech?".into());
        let data = encode_install(&partial, "_forever_", "wow");
        assert!(!message(&Reader::new(&data).fields().unwrap(), 3).unwrap().unwrap().contains_key(&9));
        // Not WoW: no HighRes, and its own family.
        let data = encode_install(&completed, "", "s2");
        let fields = Reader::new(&data).fields().unwrap();
        assert!(!message(&fields, 3).unwrap().unwrap().contains_key(&9));
        assert_eq!(string(&fields, 6).as_deref(), Some("s2"));
    }

    #[test]
    fn fills_in_agent_defaults() {
        let bare = ProductInstall {
            uid: "wowf".into(),
            product_code: "wowf".into(),
            install_path: "/x".into(),
            ..Default::default()
        };
        let data = encode_install(&bare, "_forever_", "wow");
        let parsed = parse_install(&data).unwrap().unwrap();
        assert_eq!(parsed.region.as_deref(), Some("us"));
        assert_eq!((parsed.text_language.as_deref(), parsed.speech_language.as_deref()), (Some("enUS"), Some("enUS")));
        assert_eq!(parsed.tag_string.as_deref(), Some("OSX"));
        assert_eq!((parsed.version, parsed.build_config), (None, None));
        let settings = message(&Reader::new(&data).fields().unwrap(), 3).unwrap().unwrap();
        assert_eq!(settings[&8].len(), 1);
    }
}

//! A game folder's build records, as the Battle.net Agent writes them.
//!
//! `<root>/.build.info` is a [BPSV](crate::bpsv) table with one row per
//! installed product: WoW's flavors share a root, so a root can list several
//! (`wow`, `wowf`, `wow_classic`…). `<root>/<flavor>/.flavor.info` says which
//! product a flavor folder holds.

use crate::bpsv::{Column, Table};

/// The header the Agent (and so this crate) writes.
pub const BUILD_INFO_HEADER: &str = "Branch!STRING:0|Active!DEC:1|Build Key!HEX:16|CDN Key!HEX:16|Install Key!HEX:16|\
IM Size!DEC:4|CDN Path!STRING:0|CDN Hosts!STRING:0|CDN Servers!STRING:0|Tags!STRING:0|Armadillo!STRING:0|\
Last Activated!STRING:0|Version!STRING:0|KeyRing!HEX:16|Product!STRING:0";

/// One product's row: the build installed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfoRow {
    /// The play region (`eu`); written as `Branch`, in lowercase.
    pub region: String,
    /// The build config's hex key.
    pub build_key: String,
    /// The CDN config's hex key.
    pub cdn_key: String,
    /// The `cdns` table's `Path`, `Hosts` (space-separated) and `Servers`.
    pub cdn_path: String,
    pub cdn_hosts: String,
    pub cdn_servers: String,
    pub tags: String,
    /// The `versions` table's `VersionsName`.
    pub version: String,
    pub key_ring: Option<String>,
    /// The product code (`wowf`).
    pub product: String,
}

impl BuildInfoRow {
    fn cells(&self) -> Vec<String> {
        [
            self.region.to_ascii_lowercase().as_str(),
            "1",
            &self.build_key,
            &self.cdn_key,
            "",
            "",
            &self.cdn_path,
            &self.cdn_hosts,
            &self.cdn_servers,
            &self.tags,
            "",
            "",
            &self.version,
            self.key_ring.as_deref().unwrap_or_default(),
            &self.product,
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

/// The `.build.info` to write for `row`: the existing file's rows for other
/// products kept, the row for `row.product` replaced (rows are matched by
/// their `Product` column). Rows under a different header are rewritten
/// under this one by column name; under the same header they keep their
/// bytes.
pub fn write_build_info(existing: Option<&str>, row: &BuildInfoRow) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(existing) = existing {
        let old = Table::parse(existing);
        let keeps = |cells: &[String]| match old.column_index("Product") {
            Some(index) => cells.get(index).is_none_or(|product| *product != row.product),
            None => true,
        };
        let same_header =
            existing.lines().find(|line| !line.is_empty() && !line.starts_with('#')) == Some(BUILD_INFO_HEADER);
        let columns = Table::parse(BUILD_INFO_HEADER);
        for old_row in old.rows().filter(|old_row| keeps(old_row.cells())) {
            let cells = if same_header {
                old_row.cells().to_vec()
            } else {
                columns
                    .columns()
                    .iter()
                    .map(|column| old_row.get(&column.name).unwrap_or_default().to_owned())
                    .collect()
            };
            lines.push(cells.join("|"));
        }
    }
    lines.push(row.cells().join("|"));
    let mut out = String::from(BUILD_INFO_HEADER);
    for line in lines {
        out.push('\n');
        out.push_str(&line);
    }
    out.push('\n');
    out
}

/// `.flavor.info` for a flavor folder holding `product`.
pub fn flavor_info(product: &str) -> String {
    let mut table = Table::new(vec![Column::new("Product Flavor", "STRING:0")]);
    table.push_row(vec![product.to_owned()]);
    table.to_string()
}

/// The tags `.build.info` records for an install, when exactly one active
/// row matches it: one of its product codes (`products`: the uid and the
/// product code), its region, its build (`build_key`, else `version`), and a
/// macOS selection. Never another flavor's, region's or build's tags from a
/// shared root.
pub fn installed_tags(
    text: &str,
    products: &[&str],
    region: &str,
    build_key: Option<&str>,
    version: Option<&str>,
) -> Option<String> {
    let table = Table::parse(text);
    let mut selections: Vec<&str> = table
        .rows()
        .filter(|row| row.get("Active") == Some("1"))
        .filter(|row| row.get("Product").is_some_and(|product| products.contains(&product)))
        .filter(|row| row.get("Branch").is_some_and(|branch| branch.eq_ignore_ascii_case(region)))
        .filter(|row| match (build_key, version) {
            (Some(build), _) => row.get("Build Key").is_some_and(|key| key.eq_ignore_ascii_case(build)),
            (None, Some(version)) => row.get("Version") == Some(version),
            (None, None) => false,
        })
        .filter_map(|row| row.non_empty("Tags"))
        .filter(|tags| is_mac_selection(tags))
        .collect();
    selections.sort_unstable();
    selections.dedup();
    match selections.as_slice() {
        [tags] => Some((*tags).to_owned()),
        _ => None,
    }
}

/// Whether a tag string selects macOS files: it names `OSX` and no Windows
/// platform (Overwatch's Windows manifests use `SPWin`/`TPWin`).
fn is_mac_selection(tags: &str) -> bool {
    let words: Vec<&str> = tags.split(|c: char| c.is_whitespace() || c == ':').collect();
    words.contains(&"OSX") && !words.iter().any(|word| ["Windows", "SPWin", "TPWin"].contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(product: &str, build: &str) -> BuildInfoRow {
        BuildInfoRow {
            region: "EU".into(),
            build_key: build.into(),
            cdn_key: "c".repeat(32),
            cdn_path: "tpr/wow".into(),
            cdn_hosts: "eu.cdn.blizzard.com level3.blizzard.com".into(),
            cdn_servers: "https://eu.cdn.blizzard.com/?maxhosts=4".into(),
            tags: "OSX x86_64 arm64 EU? enUS speech?:OSX x86_64 arm64 EU? enUS text?".into(),
            version: "1.60.1.70862".into(),
            key_ring: None,
            product: product.into(),
        }
    }

    #[test]
    fn writes_a_new_build_info_like_the_agent() {
        let text = write_build_info(None, &row("wowf", &"a".repeat(32)));
        let expected = format!(
            "{BUILD_INFO_HEADER}\neu|1|{}|{}|||tpr/wow|eu.cdn.blizzard.com level3.blizzard.com|\
             https://eu.cdn.blizzard.com/?maxhosts=4|OSX x86_64 arm64 EU? enUS speech?:OSX x86_64 arm64 EU? enUS text?|||\
             1.60.1.70862||wowf\n",
            "a".repeat(32),
            "c".repeat(32)
        );
        assert_eq!(text, expected);
        let table = Table::parse(&text);
        assert_eq!(table.columns().len(), 15);
        assert_eq!(table.row(0).unwrap().get("Product"), Some("wowf"));
        assert_eq!(table.row(0).unwrap().get("IM Size"), Some(""));
    }

    /// WoW's flavors share `.build.info`: an update of one replaces its own
    /// row and keeps the others' bytes.
    #[test]
    fn keeps_other_products_and_replaces_its_own() {
        let first = write_build_info(None, &row("wow", &"1".repeat(32)));
        let both = write_build_info(Some(&first), &row("wowf", &"2".repeat(32)));
        assert!(both.starts_with(&first));
        let updated = write_build_info(Some(&both), &row("wowf", &"3".repeat(32)));
        let table = Table::parse(&updated);
        let builds: Vec<_> = table.rows().map(|r| (r.get("Product").unwrap(), r.get("Build Key").unwrap())).collect();
        assert_eq!(builds, [("wow", "1".repeat(32).as_str()), ("wowf", "3".repeat(32).as_str())]);
        assert_eq!(updated.lines().nth(1), first.lines().nth(1));
    }

    #[test]
    fn rewrites_rows_under_another_header_by_column_name() {
        let other = "Product!STRING:0|Active!DEC:1|Build Key!HEX:16\n## seqn = 3\nwow|1|abc\nwowf|1|old\n";
        let text = write_build_info(Some(other), &row("wowf", "new"));
        let table = Table::parse(&text);
        assert_eq!(table.len(), 2);
        let wow = table.row(0).unwrap();
        assert_eq!(
            (wow.get("Product"), wow.get("Active"), wow.get("Build Key"), wow.get("Branch")),
            (Some("wow"), Some("1"), Some("abc"), Some(""))
        );
        assert_eq!(table.row(1).unwrap().get("Build Key"), Some("new"));
    }

    #[test]
    fn writes_the_flavor_info() {
        assert_eq!(flavor_info("wowf"), "Product Flavor!STRING:0\nwowf\n");
    }

    #[test]
    fn finds_the_installed_tags_of_exactly_this_install() {
        let mut eu = row("wowf", &"a".repeat(32));
        eu.tags = "OSX arm64 EU? deDE speech?:OSX arm64 EU? deDE text?".into();
        let mut retail = row("wow", &"a".repeat(32));
        retail.tags = "OSX arm64 EU? frFR text?".into();
        let text = write_build_info(Some(&write_build_info(None, &retail)), &eu);
        let build = "A".repeat(32);
        assert_eq!(installed_tags(&text, &["wowf"], "EU", Some(&build), None).as_deref(), Some(eu.tags.as_str()));
        assert_eq!(
            installed_tags(&text, &["wowf"], "eu", None, Some("1.60.1.70862")).as_deref(),
            Some(eu.tags.as_str())
        );
        assert_eq!(installed_tags(&text, &["wowf"], "us", Some(&build), None), None);
        assert_eq!(installed_tags(&text, &["wowf"], "eu", Some(&"b".repeat(32)), None), None);
        assert_eq!(installed_tags(&text, &["wowf"], "eu", None, None), None);
        // Two different selections for the same install: neither.
        assert_eq!(installed_tags(&text, &["wow", "wowf"], "eu", Some(&build), None), None);

        let mut windows = row("wowf", &"a".repeat(32));
        windows.tags = "Windows x86_64 EU? enUS text?".into();
        assert_eq!(installed_tags(&write_build_info(None, &windows), &["wowf"], "eu", Some(&build), None), None);
    }
}

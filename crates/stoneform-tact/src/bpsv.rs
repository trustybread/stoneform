//! Blizzard's pipe-separated tables (BPSV): the version service's `versions`
//! and `cdns` responses, and a game folder's `.build.info`.
//!
//! ```text
//! Region!STRING:0|BuildConfig!HEX:16|BuildId!DEC:4
//! ## seqn = 3016236
//! us|0123…|70862
//! ```
//!
//! The first line names the columns, each `Name!TYPE:size`; lines starting
//! with `#` are comments (`## seqn = N` carries the table's sequence number);
//! every other non-empty line is a row of `|`-separated cells.

use std::fmt;

/// A column: its name and its declared type (`STRING:0`, `HEX:16`, `DEC:4`),
/// kept verbatim so a table writes back the header it was read with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// What follows the `!` in the header; empty when there's no `!`.
    pub kind: String,
}

impl Column {
    pub fn new(name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self { name: name.into(), kind: kind.into() }
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.kind.is_empty() { f.write_str(&self.name) } else { write!(f, "{}!{}", self.name, self.kind) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    seqn: Option<u64>,
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Self {
        Self { columns, rows: Vec::new(), seqn: None }
    }

    /// Reads a table. Never fails: a text without a header is an empty
    /// table, and a row with fewer cells than columns lacks the last ones.
    pub fn parse(text: &str) -> Self {
        let mut table = Table::default();
        let mut has_header = false;
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                match comment.trim_start_matches('#').split_once('=') {
                    Some((name, value)) if name.trim() == "seqn" => table.seqn = value.trim().parse().ok(),
                    _ => {}
                }
                continue;
            }
            let cells = line.split('|');
            if has_header {
                table.rows.push(cells.map(str::to_owned).collect());
            } else {
                table.columns = cells
                    .map(|cell| match cell.split_once('!') {
                        Some((name, kind)) => Column::new(name, kind),
                        None => Column::new(cell, ""),
                    })
                    .collect();
                has_header = true;
            }
        }
        table
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The first column with this name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// The `## seqn` comment's number, when the table has one.
    pub fn seqn(&self) -> Option<u64> {
        self.seqn
    }

    pub fn set_seqn(&mut self, seqn: Option<u64>) {
        self.seqn = seqn;
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(move |cells| Row { table: self, cells })
    }

    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        self.rows.get(index).map(|cells| Row { table: self, cells })
    }

    /// Appends a row of cells, in column order.
    pub fn push_row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Keeps only the rows `keep` returns true for.
    pub fn retain_rows(&mut self, mut keep: impl FnMut(Row<'_>) -> bool) {
        let rows = std::mem::take(&mut self.rows);
        let kept = rows.into_iter().filter(|cells| keep(Row { table: self, cells })).collect();
        self.rows = kept;
    }
}

/// Writes the table back: header, `## seqn` when set, rows, each line ending
/// in `\n`.
impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header: Vec<String> = self.columns.iter().map(Column::to_string).collect();
        writeln!(f, "{}", header.join("|"))?;
        if let Some(seqn) = self.seqn {
            writeln!(f, "## seqn = {seqn}")?;
        }
        for row in &self.rows {
            writeln!(f, "{}", row.join("|"))?;
        }
        Ok(())
    }
}

/// One row, read by column name.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    table: &'a Table,
    cells: &'a [String],
}

impl<'a> Row<'a> {
    /// The cell under the first column named `column`; `None` when there's no
    /// such column or the row is too short for it.
    pub fn get(&self, column: &str) -> Option<&'a str> {
        let index = self.table.column_index(column)?;
        self.cells.get(index).map(String::as_str)
    }

    /// Like [`Row::get`], but an empty cell is `None` too.
    pub fn non_empty(&self, column: &str) -> Option<&'a str> {
        self.get(column).filter(|value| !value.is_empty())
    }

    pub fn cells(&self) -> &'a [String] {
        self.cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table half of Waypoint's `parsesVersionTablesAndConfigs`.
    #[test]
    fn parses_version_tables() {
        let table =
            Table::parse("Region!STRING:0|BuildConfig!HEX:16|BuildId!DEC:4\n## seqn = 1\nus|aaaa|10\neu|bbbb|11\n");
        assert_eq!(table.len(), 2);
        assert_eq!(table.row(1).unwrap().get("BuildConfig"), Some("bbbb"));
        assert_eq!(table.seqn(), Some(1));
        assert_eq!(table.columns()[2], Column::new("BuildId", "DEC:4"));
    }

    #[test]
    fn short_rows_lack_their_last_cells() {
        let table = Table::parse("A!STRING:0|B!STRING:0\r\n\r\n# comment\r\nx\r\n");
        let row = table.row(0).unwrap();
        assert_eq!(row.get("A"), Some("x"));
        assert_eq!(row.get("B"), None);
        assert_eq!(row.get("C"), None);
        assert!(Table::parse("").is_empty());
    }

    #[test]
    fn writes_back_what_it_read() {
        let text = "Branch!STRING:0|Active!DEC:1|Tags!STRING:0\n## seqn = 7\neu|1|OSX EU? enUS speech?:OSX EU? enUS text?\nus|0|\n";
        let table = Table::parse(text);
        assert_eq!(table.to_string(), text);

        let mut built = Table::new(vec![Column::new("Product Flavor", "STRING:0")]);
        built.push_row(vec!["wowf".into()]);
        assert_eq!(built.to_string(), "Product Flavor!STRING:0\nwowf\n");
        built.retain_rows(|row| row.get("Product Flavor") != Some("wowf"));
        assert!(built.is_empty());
    }
}

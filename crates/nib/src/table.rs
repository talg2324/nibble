//! CSV / TSV as a table: parsing, column metrics and sorting. The widget in `table_view` draws it.

use std::cmp::Ordering;

/// Widest a column gets, in cells; longer values are cut with an ellipsis.
pub const MAX_COL_WIDTH: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub col: usize,
    pub descending: bool,
}

pub struct Table {
    pub header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Display width of each column in cells (header and values, capped at `MAX_COL_WIDTH`).
    pub widths: Vec<usize>,
    /// Every non-empty value in the column is a number: sorted numerically, right-aligned.
    pub numeric: Vec<bool>,
    /// Row indices in display order.
    order: Vec<usize>,
    pub sort: Option<Sort>,
}

/// The separator a `.csv` really uses: the candidate that splits the first line into most fields.
fn sniff_delimiter(text: &str) -> u8 {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut best = (b',', 0);
    for d in [b',', b';', b'\t', b'|'] {
        let mut n = 0;
        let mut quoted = false;
        for b in first.bytes() {
            match b {
                b'"' => quoted = !quoted,
                _ if b == d && !quoted => n += 1,
                _ => {}
            }
        }
        if n > best.1 {
            best = (d, n);
        }
    }
    best.0
}

/// One line per cell: control characters would break the grid.
fn clean(cell: &str) -> String {
    let cell = cell.trim();
    if cell.contains(['\n', '\r', '\t']) {
        cell.replace("\r\n", "⏎").replace(['\n', '\r'], "⏎").replace('\t', " ")
    } else {
        cell.to_string()
    }
}

fn number(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_suffix('%').unwrap_or(s);
    let v: f64 = if s.contains(',') { s.replace(',', "").parse().ok()? } else { s.parse().ok()? };
    v.is_finite().then_some(v)
}

impl Table {
    /// `delimiter`: `None` sniffs it from the first line.
    pub fn parse(text: &str, delimiter: Option<u8>) -> Table {
        let delimiter = delimiter.unwrap_or_else(|| sniff_delimiter(text));
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(text.as_bytes());
        let mut records = reader.records().map_while(Result::ok).map(|r| r.iter().map(clean).collect::<Vec<_>>());
        let mut header = records.next().unwrap_or_default();
        let rows: Vec<Vec<String>> = records.collect();
        let ncols = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(0);
        header.resize(ncols, String::new());

        let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
        let mut numeric = vec![true; ncols];
        let mut seen = vec![false; ncols];
        for row in &rows {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(cell.chars().count());
                if !cell.is_empty() {
                    seen[c] = true;
                    numeric[c] = numeric[c] && number(cell).is_some();
                }
            }
        }
        for c in 0..ncols {
            // +2: room for the sort arrow in the header.
            widths[c] = widths[c].max(header[c].chars().count() + 2).clamp(3, MAX_COL_WIDTH);
            numeric[c] &= seen[c];
        }
        let order = (0..rows.len()).collect();
        Table { header, rows, widths, numeric, order, sort: None }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn columns(&self) -> usize {
        self.header.len()
    }

    /// The row shown at display position `i`: its 1-based number in the file and its cells.
    pub fn row(&self, i: usize) -> Option<(usize, &[String])> {
        let r = *self.order.get(i)?;
        Some((r + 1, &self.rows[r]))
    }

    /// Header click: ascending, then descending, then back to file order.
    pub fn cycle_sort(&mut self, col: usize) {
        let next = match self.sort {
            Some(Sort { col: c, descending: false }) if c == col => Some(Sort { col, descending: true }),
            Some(Sort { col: c, descending: true }) if c == col => None,
            _ => Some(Sort { col, descending: false }),
        };
        self.set_sort(next);
    }

    pub fn set_sort(&mut self, sort: Option<Sort>) {
        self.sort = sort.filter(|s| s.col < self.columns());
        self.order = (0..self.rows.len()).collect();
        let Some(Sort { col, descending }) = self.sort else { return };
        let numeric = self.numeric[col];
        let rows = &self.rows;
        let cell = |r: usize| rows[r].get(col).map(String::as_str).unwrap_or("");
        // Stable, so equal values keep file order. Empty cells go last in both directions.
        self.order.sort_by(|&a, &b| {
            let (x, y) = (cell(a), cell(b));
            match (x.is_empty(), y.is_empty()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => {}
            }
            let ord = if numeric {
                number(x).partial_cmp(&number(y)).unwrap_or(Ordering::Equal)
            } else {
                x.to_lowercase().cmp(&y.to_lowercase()).then_with(|| x.cmp(y))
            };
            if descending {
                ord.reverse()
            } else {
                ord
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(t: &Table, c: usize) -> Vec<&str> {
        (0..t.len()).map(|i| t.row(i).unwrap().1.get(c).map(String::as_str).unwrap_or("")).collect()
    }

    const SRC: &str = "name,qty,note\npear,10,\"a, b\"\nApple,9,\nfig,,\"two\nlines\"\nbanana,100,x\n";

    #[test]
    fn parses_quotes_and_ragged_rows() {
        let t = Table::parse(SRC, None);
        assert_eq!(t.header, ["name", "qty", "note"]);
        assert_eq!(t.len(), 4);
        assert_eq!(t.row(0).unwrap(), (1, &["pear".to_string(), "10".into(), "a, b".into()][..]));
        assert_eq!(t.row(2).unwrap().1[2], "two⏎lines");
        assert_eq!(t.numeric, [false, true, false]);

        let ragged = Table::parse("a,b\n1\n1,2,3\n", None);
        assert_eq!(ragged.columns(), 3);
        assert_eq!(ragged.header, ["a", "b", ""]);
        assert_eq!(Table::parse("", None).columns(), 0);
    }

    #[test]
    fn sniffs_the_delimiter() {
        assert_eq!(Table::parse("a;b;c\n1;2;3\n", None).columns(), 3);
        assert_eq!(Table::parse("a\tb\n1\t2\n", None).columns(), 2);
        assert_eq!(Table::parse("\"x;y\",b\n1,2\n", None).columns(), 2);
        // An explicit delimiter (TSV) is not second-guessed.
        assert_eq!(Table::parse("a,b\tc\n", Some(b'\t')).header, ["a,b", "c"]);
    }

    #[test]
    fn sorts_numbers_numerically_and_text_without_case() {
        let mut t = Table::parse(SRC, None);
        t.cycle_sort(1);
        assert_eq!(t.sort, Some(Sort { col: 1, descending: false }));
        assert_eq!(col(&t, 1), ["9", "10", "100", ""]);
        t.cycle_sort(1);
        assert_eq!(col(&t, 1), ["100", "10", "9", ""]);
        // The file row number travels with the row.
        assert_eq!(t.row(0).unwrap().0, 4);
        t.cycle_sort(1);
        assert_eq!(t.sort, None);
        assert_eq!(col(&t, 0), ["pear", "Apple", "fig", "banana"]);

        t.cycle_sort(0);
        assert_eq!(col(&t, 0), ["Apple", "banana", "fig", "pear"]);
        // Clicking another column starts ascending there.
        t.cycle_sort(2);
        assert_eq!(t.sort, Some(Sort { col: 2, descending: false }));
    }

    #[test]
    fn widths_are_capped() {
        let long = "x".repeat(200);
        let t = Table::parse(&format!("h\n{long}\n"), None);
        assert_eq!(t.widths, [MAX_COL_WIDTH]);
        assert!(number("1,234.5").is_some() && number("12%").is_some() && number("n/a").is_none());
    }
}

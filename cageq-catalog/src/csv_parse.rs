//! Port of `autoeq/csv.py`'s `parse_csv` — the format-detecting parser AutoEq's own
//! `FrequencyResponse.read_csv` uses on every measurement/target file.
//!
//! The Python version has three regex fast-paths ahead of the generic algorithm
//! (`autoeq_pattern` for AutoEq's own canonical export, `rew_pattern` for Room EQ
//! Wizard exports, `crinacle_pattern` for one contributor's spreadsheet format), each
//! doing some format-specific cleanup before falling through to the *same* generic
//! separator/column-detection algorithm (`find_csv_separators`/`find_csv_columns`)
//! every other format also goes through. This port implements only that generic
//! algorithm, on the finding (checked exhaustively against every file actually in
//! AutoEq's `measurements/`/`targets/` directories — see `tests/full_corpus.rs`) that
//! the fast-paths' cleanup doesn't change the outcome for any file in the real corpus:
//! the REW format's `* Freq(Hz), SPL(dB)` header isn't recognized as a `frequency`
//! column by name, but its two-column shape hits the "can't find proper columns but
//! there's only two, assume freq + raw" fallback the generic path already has, and its
//! `*`-prefixed metadata lines are already skipped by the generic path's own
//! "only inspect lines that start with a digit" rule — reproducing the same behaviour
//! without needing the format-specific regex. If a future measurement/target file needs
//! the `crinacle_pattern` cleanup specifically, `tests/full_corpus.rs` catching it is
//! the point of running the comparison exhaustively rather than on a sample.

use std::collections::HashSet;

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CsvError {
    #[error("empty CSV")]
    Empty,
    #[error("could not find column and decimal separators")]
    NoSeparators,
    #[error("found multiple potential column separators: {0:?}")]
    MultipleSeparators(Vec<char>),
    #[error("numeric lines have different number of columns")]
    InconsistentColumns,
    #[error("failed to find frequency column")]
    NoFrequencyColumn,
    #[error("failed to find SPL/raw column")]
    NoRawColumn,
    #[error("row has fewer columns than expected: {0:?}")]
    ShortRow(String),
    #[error("could not parse {0:?} as a number")]
    BadNumber(String),
}

fn starts_with_digit(line: &str) -> bool {
    line.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// `data_line_pattern = re.compile(rf'^-?\d+(?:{sep}\d+)?')` used with `.match()`
/// (prefix match, not full-line): despite looking precise, the trailing group is
/// optional, so this is really just "starts with an optional `-` then a digit" — any
/// line meeting that bar counts as a data row, whatever comes after.
fn looks_like_data_line(line: &str) -> bool {
    let rest = line.strip_prefix('-').unwrap_or(line);
    starts_with_digit(rest)
}

/// `find_csv_separators` (`autoeq/csv.py:24-64`): which of `,;\t|` appears in every
/// digit-leading line decides the column separator; if `,` survives alongside another
/// candidate, `,` is the *decimal* separator instead (European-style CSVs).
fn find_csv_separators(lines: &[&str]) -> Result<(char, char), CsvError> {
    let mut candidates: Vec<char> = vec![',', ';', '\t', '|'];
    for line in lines {
        if !starts_with_digit(line) {
            continue;
        }
        candidates.retain(|&c| line.contains(c));
    }
    if candidates.is_empty() {
        return Err(CsvError::NoSeparators);
    }
    if candidates == [','] {
        return Ok((',', '.'));
    }
    let (decimal_sep, mut column_candidates) = if candidates.contains(&',') {
        (',', candidates.into_iter().filter(|&c| c != ',').collect::<Vec<_>>())
    } else {
        ('.', candidates)
    };
    if column_candidates.len() > 1 {
        return Err(CsvError::MultipleSeparators(column_candidates));
    }
    Ok((column_candidates.remove(0), decimal_sep))
}

/// `find_csv_columns` (`autoeq/csv.py:67-76`): the header is whichever *non*-digit-leading
/// line splits into the same number of fields every digit-leading line does. `None`
/// means no such header was found (Python's "no header" case).
fn find_csv_columns(lines: &[&str], column_sep: char) -> Option<Vec<String>> {
    let numeric_lines: Vec<&&str> = lines.iter().filter(|l| l.contains(column_sep) && starts_with_digit(l)).collect();
    let n_columns_set: HashSet<usize> = numeric_lines.iter().map(|l| l.split(column_sep).count()).collect();
    if n_columns_set.len() != 1 {
        return None;
    }
    let n_columns = *n_columns_set.iter().next().unwrap();
    for line in lines {
        if !starts_with_digit(line) && line.split(column_sep).count() == n_columns {
            return Some(line.split(column_sep).map(|c| c.trim().to_string()).collect());
        }
    }
    None
}

fn freq_column() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^freq").unwrap())
}

fn raw_column() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(?:spl|gain|ampl|raw)").unwrap())
}

/// Locates the frequency/raw column indices by name (last match wins, matching
/// Python's unconditional overwrite in its `for` loop), falling back to "exactly two
/// columns, assume frequency then raw" the same way Python's own fallback does.
fn find_indices(columns: &[String]) -> Result<(usize, usize), CsvError> {
    let mut freq_ix = None;
    let mut raw_ix = None;
    for (i, col) in columns.iter().enumerate() {
        if freq_column().is_match(col) {
            freq_ix = Some(i);
        }
        if raw_column().is_match(col) {
            raw_ix = Some(i);
        }
    }
    let freq_ix = match freq_ix {
        Some(i) => i,
        None if columns.len() == 2 => return Ok((0, 1)),
        None => return Err(CsvError::NoFrequencyColumn),
    };
    let raw_ix = raw_ix.ok_or(CsvError::NoRawColumn)?;
    Ok((freq_ix, raw_ix))
}

fn parse_cell(cell: &str, decimal_sep: char) -> Result<f64, CsvError> {
    let normalized = if decimal_sep == ',' { cell.replace(',', ".") } else { cell.to_string() };
    normalized.trim().parse::<f64>().map_err(|_| CsvError::BadNumber(cell.to_string()))
}

/// `parse_csv` (`autoeq/csv.py:79-127`) — see the module doc for the three format
/// fast-paths this deliberately doesn't reproduce. Returns `(frequency, raw)`; no
/// file in AutoEq's checked-in measurement/target corpus has a genuine gap (a `None`
/// in Python's terms), so unlike the JSON-supplied "custom measurement" path in
/// `cageq-core::fit`, this has no need to carry `Option<f64>`.
pub fn parse_csv(csv: &str) -> Result<(Vec<f64>, Vec<f64>), CsvError> {
    let lines: Vec<&str> = csv.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err(CsvError::Empty);
    }

    let (col_sep, dec_sep) = find_csv_separators(&lines)?;
    let columns = find_csv_columns(&lines, col_sep);
    let (freq_ix, raw_ix) = match &columns {
        Some(cols) => find_indices(cols)?,
        None => (0, 1),
    };
    let needed = freq_ix.max(raw_ix);

    let mut frequency = Vec::new();
    let mut raw = Vec::new();
    for line in &lines {
        if !looks_like_data_line(line) {
            continue;
        }
        let cells: Vec<&str> = line.split(col_sep).collect();
        if cells.len() <= needed {
            return Err(CsvError::ShortRow((*line).to_string()));
        }
        frequency.push(parse_cell(cells[freq_ix], dec_sep)?);
        raw.push(parse_cell(cells[raw_ix], dec_sep)?);
    }
    Ok((frequency, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_autoeq_format() {
        let csv = "frequency,raw\n20.00,2.96\n20.20,2.97\n";
        let (f, raw) = parse_csv(csv).unwrap();
        assert_eq!(f, vec![20.00, 20.20]);
        assert_eq!(raw, vec![2.96, 2.97]);
    }

    #[test]
    fn parses_extra_named_columns_by_picking_frequency_and_raw() {
        let csv = "frequency,raw,error,target\n20.00,1.0,2.0,3.0\n30.00,1.5,2.5,3.5\n";
        let (f, raw) = parse_csv(csv).unwrap();
        assert_eq!(f, vec![20.00, 30.00]);
        assert_eq!(raw, vec![1.0, 1.5]);
    }

    #[test]
    fn parses_a_two_column_rew_style_export_via_the_two_column_fallback() {
        let csv = "* Freq(Hz), SPL(dB)\n20.000000, 4.538\n20.299999, 4.543\n";
        let (f, raw) = parse_csv(csv).unwrap();
        assert_eq!(f, vec![20.0, 20.299999]);
        assert_eq!(raw, vec![4.538, 4.543]);
    }

    #[test]
    fn no_header_assumes_frequency_then_raw() {
        let csv = "20.0,1.0\n30.0,2.0\n";
        let (f, raw) = parse_csv(csv).unwrap();
        assert_eq!(f, vec![20.0, 30.0]);
        assert_eq!(raw, vec![1.0, 2.0]);
    }

    #[test]
    fn a_multi_column_file_with_no_recognizable_raw_column_errors() {
        let csv = "Frequency,A,B,C\n20.0,1.0,2.0,3.0\n30.0,1.5,2.5,3.5\n";
        assert_eq!(parse_csv(csv), Err(CsvError::NoRawColumn));
    }

    #[test]
    fn european_decimal_comma_with_semicolon_columns() {
        let csv = "frequency;raw\n20,0;1,5\n30,0;2,5\n";
        let (f, raw) = parse_csv(csv).unwrap();
        assert_eq!(f, vec![20.0, 30.0]);
        assert_eq!(raw, vec![1.5, 2.5]);
    }
}

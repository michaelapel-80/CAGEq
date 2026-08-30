//! Persistent per-endpoint configuration: what the APO applies when nobody is driving it.
//!
//! ## Why this exists at all
//! Equalizer APO reads `config.txt` at load, so a user's correction is live from boot
//! whether or not any GUI is running. An APO fed only by CAGEq's control channel would
//! apply EQ *only while the app is open* — a behavioural regression, not a design choice.
//! So the correction is persisted per endpoint and loaded at `LockForProcess`; the live
//! control channel (stage C3b) then layers on top of it, for edit latency only.
//!
//! ## Format
//! A tiny versioned line format, deliberately not EqAPO's — we would only be reimplementing
//! a parser for a syntax we no longer need:
//!
//! ```text
//! cageq-apo 1
//! preamp -6.5
//! band PK 105 3.0 0.70
//! band LSC 1000 -2.4 1.40
//! ```
//!
//! Filter tokens are the ones CAGEq already writes (`PK`/`LSC`/`HSC`/`BP`, AutoEq's), so
//! the same vocabulary reads the same everywhere in the project.
//!
//! ## Parsing posture: strict, and all-or-nothing
//! This is read inside `audiodg`, and the result is applied to what someone is listening to.
//! A half-understood file must never become a half-applied correction: any malformed line
//! rejects the whole file, and the caller keeps whatever was already running. Unknown
//! *directives* are likewise an error rather than skipped — silently ignoring a line written
//! by a newer CAGEq would apply a correction that is quietly not the one requested. The
//! version header is what a future format change moves.

use std::path::{Path, PathBuf};

use crate::dsp::{Band, FilterKind, MAX_BANDS};

/// Format version this parser accepts.
const FORMAT_VERSION: u32 = 1;
/// First token of the header line.
const MAGIC: &str = "cageq-apo";

/// A parsed configuration: exactly what to hand the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ApoConfig {
    pub preamp_db: f64,
    pub bands: Vec<Band>,
}

impl Default for ApoConfig {
    /// No correction — the neutral state, and what an absent config file means.
    fn default() -> Self {
        ApoConfig { preamp_db: 0.0, bands: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number, so a complaint can be acted on.
    pub line: usize,
    pub reason: &'static str,
}

/// Where an endpoint's configuration lives.
///
/// `%ProgramData%\CAGEq\apo\<endpoint-guid>.cfg` — chosen because `audiodg` runs as
/// LocalService in session 0 and can read it there (the same reason EqualizerAPO's own
/// config works from that process), while an unelevated CAGEq can be granted write access to
/// this one directory at install time. Per endpoint, because a correction is scoped to a
/// device, exactly as `cageq.txt`'s `Device:` blocks are.
pub fn config_path(endpoint_id: &str) -> PathBuf {
    let root = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    // The id is a registry-style GUID in braces; keep it verbatim so the filename can be
    // matched against the endpoint list by eye, but refuse anything with path separators in
    // it — this string ultimately comes from outside and must not be able to escape.
    root.join("CAGEq").join("apo").join(format!("{endpoint_id}.cfg"))
}

/// Is `endpoint_id` a plausible endpoint GUID? Guards [`load`] against a value that could
/// traverse out of the config directory, since the id reaches us from the audio engine
/// rather than from our own code.
pub fn is_valid_endpoint_id(endpoint_id: &str) -> bool {
    !endpoint_id.is_empty()
        && endpoint_id.len() <= 64
        && endpoint_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '{' | '}' | '-'))
}

/// Read and parse an endpoint's configuration.
///
/// `Ok(None)` means "no configuration for this endpoint", which is a normal state (a device
/// CAGEq has never been pointed at) and means *apply nothing*, not *fail*. `Err` means a
/// file exists but could not be trusted — the caller should keep running whatever it had.
pub fn load(endpoint_id: &str) -> Result<Option<ApoConfig>, ParseError> {
    if !is_valid_endpoint_id(endpoint_id) {
        return Err(ParseError { line: 0, reason: "implausible endpoint id" });
    }
    load_from(&config_path(endpoint_id))
}

/// [`load`] against an explicit path — the testable half.
pub fn load_from(path: &Path) -> Result<Option<ApoConfig>, ParseError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some),
        // Absent (or unreadable — a permissions slip is not a reason to make noise on the
        // audio path) is simply "no correction here".
        Err(_) => Ok(None),
    }
}

/// Parse the config format. Pure, so the whole decision table is testable without a file.
pub fn parse(text: &str) -> Result<ApoConfig, ParseError> {
    let mut config = ApoConfig::default();
    let mut seen_header = false;
    let mut seen_preamp = false;

    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let no = i + 1;

        // Blank lines and `#` comments are the only things skipped.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut tok = line.split_whitespace();
        let directive = tok.next().unwrap_or_default();

        if !seen_header {
            if directive != MAGIC {
                return Err(ParseError { line: no, reason: "expected a 'cageq-apo <version>' header" });
            }
            match tok.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(FORMAT_VERSION) => {}
                Some(_) => return Err(ParseError { line: no, reason: "unsupported format version" }),
                None => return Err(ParseError { line: no, reason: "malformed version" }),
            }
            if tok.next().is_some() {
                return Err(ParseError { line: no, reason: "trailing tokens after the header" });
            }
            seen_header = true;
            continue;
        }

        match directive {
            "preamp" => {
                if seen_preamp {
                    return Err(ParseError { line: no, reason: "duplicate preamp" });
                }
                let db = parse_finite(tok.next(), no, "preamp")?;
                config.preamp_db = db;
                seen_preamp = true;
            }
            "band" => {
                if config.bands.len() == MAX_BANDS {
                    return Err(ParseError { line: no, reason: "too many bands" });
                }
                let kind = match tok.next() {
                    Some("PK") => FilterKind::Peaking,
                    Some("LSC") => FilterKind::LowShelf,
                    Some("HSC") => FilterKind::HighShelf,
                    Some("BP") => FilterKind::Bandpass,
                    _ => return Err(ParseError { line: no, reason: "unknown filter type" }),
                };
                let freq_hz = parse_finite(tok.next(), no, "frequency")?;
                let gain_db = parse_finite(tok.next(), no, "gain")?;
                let q = parse_finite(tok.next(), no, "Q")?;
                // The same limits the FFI enforces: a non-positive frequency or Q yields NaN
                // coefficients, and NaN in a biquad's delay registers is permanent.
                if freq_hz <= 0.0 {
                    return Err(ParseError { line: no, reason: "frequency must be positive" });
                }
                if q <= 0.0 {
                    return Err(ParseError { line: no, reason: "Q must be positive" });
                }
                config.bands.push(Band { kind, freq_hz, gain_db, q });
            }
            _ => return Err(ParseError { line: no, reason: "unknown directive" }),
        }

        if tok.next().is_some() {
            return Err(ParseError { line: no, reason: "trailing tokens" });
        }
    }

    if !seen_header {
        return Err(ParseError { line: 0, reason: "empty or headerless file" });
    }
    Ok(config)
}

fn parse_finite(tok: Option<&str>, line: usize, what: &'static str) -> Result<f64, ParseError> {
    let _ = what;
    match tok.and_then(|t| t.parse::<f64>().ok()) {
        Some(v) if v.is_finite() => Ok(v),
        Some(_) => Err(ParseError { line, reason: "value must be finite" }),
        None => Err(ParseError { line, reason: "missing or malformed number" }),
    }
}

/// Render a configuration back to the format — the writer CAGEq's own backend will use, and
/// what makes the round trip testable.
pub fn render(config: &ApoConfig) -> String {
    let mut s = format!("{MAGIC} {FORMAT_VERSION}\npreamp {:.4}\n", config.preamp_db);
    for b in &config.bands {
        let token = match b.kind {
            FilterKind::Peaking => "PK",
            FilterKind::LowShelf => "LSC",
            FilterKind::HighShelf => "HSC",
            FilterKind::Bandpass => "BP",
        };
        s.push_str(&format!("band {token} {:.4} {:.4} {:.4}\n", b.freq_hz, b.gain_db, b.q));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "cageq-apo 1\npreamp -6.5\nband PK 105 3.0 0.7\nband HSC 8000 -2.4 1.4\n";

    #[test]
    fn parses_a_well_formed_config() {
        let c = parse(GOOD).expect("should parse");
        assert_eq!(c.preamp_db, -6.5);
        assert_eq!(c.bands.len(), 2);
        assert_eq!(c.bands[0].kind, FilterKind::Peaking);
        assert_eq!(c.bands[0].freq_hz, 105.0);
        assert_eq!(c.bands[1].kind, FilterKind::HighShelf);
        assert_eq!(c.bands[1].gain_db, -2.4);
    }

    /// Comments, blank lines and CRLF must all survive — a config file will be hand-edited
    /// and will cross tools that rewrite line endings.
    #[test]
    fn tolerates_comments_blank_lines_and_crlf() {
        let text = "# CAGEq APO config\r\ncageq-apo 1\r\n\r\n# bass\r\nband PK 105 3.0 0.7\r\n";
        let c = parse(text).expect("should parse");
        assert_eq!(c.bands.len(), 1);
        assert_eq!(c.preamp_db, 0.0, "an omitted preamp is 0 dB, not an error");
    }

    /// Everything that must be refused, and refused *wholly* — this is applied to what
    /// someone is listening to, so a partially-understood file cannot become a partially
    /// applied correction.
    #[test]
    fn malformed_configs_are_rejected_whole() {
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            ("preamp -6\n", "no header"),
            ("cageq-apo 2\n", "future version"),
            ("cageq-apo x\n", "malformed version"),
            ("cageq-apo 1 extra\n", "trailing header tokens"),
            ("cageq-apo 1\nband XX 100 1 1\n", "unknown filter type"),
            ("cageq-apo 1\nband PK 100 1\n", "missing Q"),
            ("cageq-apo 1\nband PK 100 1 1 9\n", "trailing tokens"),
            ("cageq-apo 1\nband PK 0 1 1\n", "zero frequency"),
            ("cageq-apo 1\nband PK -100 1 1\n", "negative frequency"),
            ("cageq-apo 1\nband PK 100 1 0\n", "zero Q"),
            ("cageq-apo 1\nband PK 100 nan 1\n", "NaN gain"),
            ("cageq-apo 1\nband PK 100 inf 1\n", "infinite gain"),
            ("cageq-apo 1\npreamp\n", "preamp without a value"),
            ("cageq-apo 1\npreamp -3\npreamp -4\n", "duplicate preamp"),
            ("cageq-apo 1\nwobble 1\n", "unknown directive"),
        ];
        for (text, why) in cases {
            assert!(parse(text).is_err(), "should have rejected: {why}");
        }
    }

    #[test]
    fn more_than_max_bands_is_rejected() {
        let mut text = String::from("cageq-apo 1\n");
        for i in 0..=MAX_BANDS {
            text.push_str(&format!("band PK {} 1.0 1.0\n", 100 + i));
        }
        let err = parse(&text).expect_err("should reject");
        assert_eq!(err.reason, "too many bands");
    }

    /// The writer and the parser must agree, or CAGEq could persist a correction its own APO
    /// then refuses to load.
    #[test]
    fn render_round_trips_through_parse() {
        let original = parse(GOOD).unwrap();
        let reparsed = parse(&render(&original)).expect("rendered config must parse");
        assert_eq!(reparsed.preamp_db, original.preamp_db);
        assert_eq!(reparsed.bands.len(), original.bands.len());
        for (a, b) in reparsed.bands.iter().zip(&original.bands) {
            assert_eq!(a.kind, b.kind);
            assert!((a.freq_hz - b.freq_hz).abs() < 1e-4);
            assert!((a.gain_db - b.gain_db).abs() < 1e-4);
            assert!((a.q - b.q).abs() < 1e-4);
        }
    }

    /// An absent config is "no correction", not a failure: a device CAGEq has never been
    /// pointed at is an ordinary state, and must not stop the APO from locking.
    #[test]
    fn an_absent_file_is_not_an_error() {
        let path = std::env::temp_dir().join("cageq-apo-does-not-exist-9e3f1a.cfg");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), Ok(None));
    }

    /// The endpoint id reaches us from the audio engine and lands in a path, so it must not
    /// be able to walk out of the config directory.
    #[test]
    fn endpoint_ids_that_could_escape_the_config_dir_are_refused() {
        assert!(is_valid_endpoint_id("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}"));
        for bad in ["", "..", r"..\..\windows\system32", "a/b", "a\\b", "a:b", &"f".repeat(65)] {
            assert!(!is_valid_endpoint_id(bad), "should refuse {bad:?}");
        }
        assert!(load("../escape").is_err());
    }

    #[test]
    fn config_path_is_per_endpoint_under_programdata() {
        let p = config_path("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}");
        assert!(p.ends_with("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}.cfg"), "{p:?}");
        assert!(p.to_string_lossy().contains("CAGEq"), "{p:?}");
    }
}

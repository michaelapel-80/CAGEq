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

// ---------------------------------------------------------------------------
// Hard limits — this file is parsed inside audiodg (LocalService, session 0) from bytes an
// UNELEVATED user can write. Everything below exists because of that asymmetry, not because
// a well-behaved CAGEq would ever emit such a value.
// ---------------------------------------------------------------------------

/// Refuse to read more than this. `read_to_string` on an attacker-chosen path is otherwise
/// an unbounded allocation *inside the audio service*: a 100 GB file in the config directory
/// would OOM or stall audiodg, taking audio down machine-wide. 64 KiB is orders of magnitude
/// above any real correction (a 32-band config is well under 2 KiB).
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Per-band gain bound, dB. Far beyond any correction CAGEq generates; the point is that a
/// corrupt or hostile file cannot ask for enough gain to be a hearing or speaker hazard.
const MAX_ABS_GAIN_DB: f64 = 40.0;
/// Preamp bounds, dB. Asymmetric on purpose: attenuation is harmless, gain is not, and
/// CAGEq's own preamp is negative in normal operation (§4.1 loudness matching).
const MIN_PREAMP_DB: f64 = -120.0;
const MAX_PREAMP_DB: f64 = 12.0;
/// Q bounds. Very high Q makes a biquad numerically fragile at low centre frequencies;
/// very low Q is meaningless. Both ends are far outside what a real correction uses.
const MIN_Q: f64 = 0.05;
const MAX_Q: f64 = 40.0;
/// Absolute frequency sanity bound, Hz. The *real* limit is Nyquist, which depends on the
/// endpoint's rate and is therefore enforced where the rate is known (see
/// `dsp::Cascade::set_bands`); this only rejects absurd values at parse time.
const MAX_FREQ_HZ: f64 = 1_000_000.0;

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

/// Canonicalise an endpoint id to the braced form Windows uses.
///
/// Exists because **PowerShell strips the braces off an unquoted `{...}`** — it parses as a
/// ScriptBlock — so a GUID typed at a prompt arrives here bare. The APO gets its id from the
/// audio engine, which always brace-wraps it, so an unnormalised writer would compute a
/// different section name and simply never find the channel. That failure looks identical to
/// "the APO isn't running", which is a genuinely expensive thing to debug.
///
/// `None` if the input is not a GUID at all, which lets a caller say so plainly instead of
/// reporting a missing channel.
pub fn normalize_endpoint_id(id: &str) -> Option<String> {
    let bare = id.trim().trim_start_matches('{').trim_end_matches('}');
    // 8-4-4-4-12 hex, the registry GUID shape.
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = bare.split('-');
    for want in groups {
        let part = parts.next()?;
        if part.len() != want || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{{{}}}", bare.to_ascii_lowercase()))
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
///
/// ## Why this is not just `read_to_string`
/// We are a service account reading a path an unelevated user can create entries in, so the
/// open itself is part of the threat model:
///
/// * **Reparse points are refused.** A user who can write the config directory can point
///   `<guid>.cfg` at any file — `C:\Windows\System32\config\SAM`, another user's documents —
///   and have LocalService open it. Creating a *directory junction* needs no special
///   privilege, so this is not a theoretical concern. `FILE_FLAG_OPEN_REPARSE_POINT` opens
///   the link itself rather than following it, and the handle is then re-checked; doing it
///   on the open handle rather than with a prior `symlink_metadata` closes the TOCTOU window
///   where the file is swapped between check and open.
/// * **Reads are capped** at [`MAX_CONFIG_BYTES`] — otherwise an oversized file is an
///   unbounded allocation inside the audio service.
/// * **Nothing read here is ever logged or returned.** Even with the above, treat the
///   content as hostile: [`ParseError`] carries a line number and a fixed reason and never
///   any bytes from the file, so this can never become an arbitrary-file-read oracle that
///   reflects privileged content back out.
///
/// None of this substitutes for ACLing the config directory at install time so that only
/// the intended account can write it — it is defence in depth, not the fence.
pub fn load_from(path: &Path) -> Result<Option<ApoConfig>, ParseError> {
    use std::io::Read;

    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    // Absent or unreadable is simply "no correction here" — a device CAGEq has never been
    // pointed at is an ordinary state, and a permissions slip is not a reason to fail a lock.
    let Ok(file) = opts.open(path) else { return Ok(None) };

    // Race-free: asked of the handle we actually opened, not of the path.
    match file.metadata() {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(ParseError { line: 0, reason: "config path is a reparse point" });
        }
        Ok(md) if md.len() > MAX_CONFIG_BYTES => {
            return Err(ParseError { line: 0, reason: "config file is implausibly large" });
        }
        Ok(_) => {}
        Err(_) => return Ok(None),
    }

    // Capped regardless of what the metadata claimed — the size can change under us, and a
    // pipe or device would report 0.
    let mut text = String::new();
    if file.take(MAX_CONFIG_BYTES).read_to_string(&mut text).is_err() {
        // Unreadable or not UTF-8. Deliberately not surfaced as content.
        return Err(ParseError { line: 0, reason: "config file is unreadable or not UTF-8" });
    }
    parse(&text).map(Some)
}

/// Parse the config format. Pure, so the whole decision table is testable without a file.
pub fn parse(text: &str) -> Result<ApoConfig, ParseError> {
    // A leading UTF-8 BOM is stripped rather than treated as part of the first token.
    // Windows produces these constantly — Notepad writes one, and PowerShell 5.1's
    // `Set-Content -Encoding UTF8` always does — so a config format that rejected them would
    // be perpetually, confusingly broken for anyone who hand-edited their correction.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

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
                // Refused, not clamped: silently turning a +60 dB request into +12 would
                // apply a correction nobody asked for. Asymmetric because attenuation is
                // harmless and gain is a hearing and speaker hazard.
                if !(MIN_PREAMP_DB..=MAX_PREAMP_DB).contains(&db) {
                    return Err(ParseError { line: no, reason: "preamp outside the safe range" });
                }
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
                // Bounded on both sides, not merely positive. A non-positive frequency or Q
                // yields NaN coefficients (and NaN in a biquad's delay registers is
                // permanent), while absurdly large values give a numerically fragile or
                // outright unstable filter — whose output grows without bound, which on
                // someone's headphones is a safety problem before it is a correctness one.
                // The real frequency ceiling is Nyquist and is enforced where the sample
                // rate is known (`dsp::Cascade::set_bands`); this is the absolute sanity gate.
                if !(freq_hz > 0.0 && freq_hz < MAX_FREQ_HZ) {
                    return Err(ParseError { line: no, reason: "frequency outside the sane range" });
                }
                if !(MIN_Q..=MAX_Q).contains(&q) {
                    return Err(ParseError { line: no, reason: "Q outside the sane range" });
                }
                if gain_db.abs() > MAX_ABS_GAIN_DB {
                    return Err(ParseError { line: no, reason: "gain outside the safe range" });
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

    /// PowerShell strips the braces off an unquoted `{...}`, so a GUID typed at a prompt
    /// arrives bare — while the APO's own id comes from the audio engine already braced. Both
    /// must name the same section, or the writer silently never finds the channel and the
    /// failure looks exactly like "the APO isn't running". That is how the first VM attempt
    /// at the control channel failed.
    #[test]
    fn bare_and_braced_guids_normalise_to_the_same_id() {
        let braced = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
        let bare = "6cafe423-cde5-4ec1-a1e2-e3fcec778349";
        assert_eq!(normalize_endpoint_id(bare), normalize_endpoint_id(braced));
        assert_eq!(normalize_endpoint_id(braced).as_deref(), Some(braced));
        // Case is normalised too, so two spellings of one endpoint cannot become two sections.
        assert_eq!(normalize_endpoint_id("{6CAFE423-CDE5-4EC1-A1E2-E3FCEC778349}").as_deref(), Some(braced));
    }

    /// Things that are not GUIDs must be rejected as such, so a caller can say "that is not an
    /// endpoint" rather than "no channel" — the mis-parsed command line that produced
    /// `-encodedCommand` was reported as a missing channel, which blamed the wrong component.
    #[test]
    fn non_guids_are_refused_rather_than_normalised() {
        for bad in [
            "-encodedCommand",
            "",
            "{}",
            "6cafe423-cde5-4ec1-a1e2",                    // too few groups
            "6cafe423-cde5-4ec1-a1e2-e3fcec778349-extra", // too many
            "6cafe42z-cde5-4ec1-a1e2-e3fcec778349",       // not hex
            "../escape",
        ] {
            assert!(normalize_endpoint_id(bad).is_none(), "accepted {bad:?}");
        }
    }

    /// A UTF-8 BOM must not break the header. Windows writes these routinely — Notepad does,
    /// and PowerShell 5.1's `Set-Content -Encoding UTF8` always does — so rejecting them made
    /// every script-written correction fail with a header error pointing nowhere near the
    /// real cause. This is exactly how the first VM run of the config path failed.
    #[test]
    fn a_utf8_bom_does_not_break_the_header() {
        let c = parse("\u{feff}cageq-apo 1\npreamp -12\nband PK 120 12 1.0\n")
            .expect("a BOM-prefixed config must parse");
        assert_eq!(c.preamp_db, -12.0);
        assert_eq!(c.bands.len(), 1);
    }

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

    /// Bounds that exist because this file is written by an unelevated user and parsed by a
    /// service account. Several are safety limits as much as security ones: an unstable or
    /// enormously loud filter reaches someone's ears before it reaches a debugger.
    #[test]
    fn parameters_outside_the_safe_ranges_are_refused() {
        let cases: &[(&str, &str)] = &[
            ("cageq-apo 1\npreamp 40\n", "preamp far above unity"),
            ("cageq-apo 1\npreamp -200\n", "preamp below the floor"),
            ("cageq-apo 1\nband PK 100 60 1\n", "gain above the cap"),
            ("cageq-apo 1\nband PK 100 -60 1\n", "gain below the cap"),
            ("cageq-apo 1\nband PK 100 1 1e9\n", "absurd Q"),
            ("cageq-apo 1\nband PK 100 1 0.0001\n", "meaningless Q"),
            ("cageq-apo 1\nband PK 1e300 1 1\n", "absurd frequency"),
        ];
        for (text, why) in cases {
            assert!(parse(text).is_err(), "should have refused: {why}");
        }
        // …while a realistic correction still parses.
        assert!(parse("cageq-apo 1\npreamp -9.5\nband PK 105 6.0 0.7\n").is_ok());
    }

    /// An oversized file must not be read into memory: this parse happens inside audiodg, so
    /// an unbounded allocation there is a machine-wide audio outage, trivially triggered by
    /// anyone who can write the config directory.
    #[test]
    fn an_oversized_config_is_refused_without_being_read() {
        let path = std::env::temp_dir().join(format!("cageq-apo-huge-{}.cfg", std::process::id()));
        let mut text = String::from("cageq-apo 1\n");
        while text.len() as u64 <= MAX_CONFIG_BYTES {
            text.push_str("# padding padding padding padding padding padding padding\n");
        }
        std::fs::write(&path, &text).unwrap();

        let err = load_from(&path).expect_err("oversized config must be refused");
        assert_eq!(err.reason, "config file is implausibly large");
        let _ = std::fs::remove_file(&path);
    }

    /// Errors must never carry bytes from the file. Otherwise, combined with a reparse point
    /// aimed at a privileged file, this becomes an arbitrary-file-read oracle that reflects
    /// content back to a caller who could not otherwise read it.
    #[test]
    fn errors_never_leak_file_content() {
        let secret = "cageq-apo 1\nband PK 100 1 1 SUPERSECRETVALUE\n";
        let err = parse(secret).expect_err("should reject");
        assert!(!err.reason.contains("SUPERSECRET"), "error leaked file content: {}", err.reason);
        // `reason` is `&'static str`, so it structurally cannot contain runtime data — this
        // test guards the property against someone later changing it to a String.
        let _: &'static str = err.reason;
    }

    #[test]
    fn config_path_is_per_endpoint_under_programdata() {
        let p = config_path("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}");
        assert!(p.ends_with("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}.cfg"), "{p:?}");
        assert!(p.to_string_lossy().contains("CAGEq"), "{p:?}");
    }
}

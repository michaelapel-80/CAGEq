//! Fetches, browses and caches AutoEq's measurement/target catalogue directly from
//! GitHub — everything `cageq-core` needed the Python sidecar for on the catalogue
//! side (`fetch_raw_curves`, `list_headphones`, `list_targets`, `measurement_curves`).
//!
//! Two halves: [`fetch_curve`] answers "given a known catalogue path, what's the
//! curve" (a plain file fetch + CSV parse); [`index`] answers "what paths exist at
//! all" (a GitHub tree-API index build, with each measurement source's own
//! `name_index.tsv` enriching the result with its rig). A headphone/target picked
//! from [`index::build_index`]/[`index::list_targets`] is exactly the path
//! [`fetch_curve`] expects.

mod csv_parse;
pub mod index;

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

pub use csv_parse::{parse_csv, CsvError};

const GH_RAW: &str = "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("network error fetching {url}: {source}")]
    Http { url: String, source: Box<ureq::Error> },
    /// A 403 with `X-RateLimit-Remaining: 0`, or a 429 — distinguished from a generic
    /// [`CatalogError::Http`] (a 404, DNS failure, etc.) so callers can tell "you're
    /// rate-limited, try later" from "something is actually broken".
    #[error("GitHub rate limit hit fetching {url}{}", retry_after.as_deref().map(|s| format!(" (retry after {s})")).unwrap_or_default())]
    RateLimited { url: String, retry_after: Option<String> },
    #[error("cache I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV parse error: {0}")]
    Csv(#[from] CsvError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// GitHub's recursive tree API silently cuts a listing short past ~100k
    /// entries/~7MB and sets `"truncated": true` instead of failing the request.
    #[error("GitHub truncated the recursive tree listing for {sha} — catalogue would be incomplete")]
    TruncatedTree { sha: String },
}

/// `_cache_dir` (`sidecar_dsp.py:99-102`): `$CAGEQ_CACHE_DIR`, else the system temp
/// dir's `cageq-cache` subdirectory — the *same* directory the Python sidecar already
/// uses, so a file fetched by either side stays cached for the other.
pub(crate) fn cache_dir() -> std::io::Result<PathBuf> {
    let d = std::env::var("CAGEQ_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("cageq-cache"));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Matches `urllib.parse.quote`'s default (`safe='/'`): percent-encode everything
/// except unreserved characters and the path separator, segment by segment so `/`
/// itself is never encoded.
fn urlencode_path(path: &str) -> String {
    path.split('/').map(percent_encode_segment).collect::<Vec<_>>().join("/")
}

fn percent_encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn http_get_binary(url: &str) -> Result<Vec<u8>, CatalogError> {
    let resp = ureq::get(url)
        .set("User-Agent", "CAGEq")
        .set("Accept", "application/vnd.github+json")
        .timeout(HTTP_TIMEOUT)
        .call()
        .map_err(|e| match &e {
            ureq::Error::Status(code, resp)
                if *code == 429 || (*code == 403 && resp.header("x-ratelimit-remaining") == Some("0")) =>
            {
                let retry_after = resp.header("retry-after").or_else(|| resp.header("x-ratelimit-reset")).map(str::to_string);
                CatalogError::RateLimited { url: url.to_string(), retry_after }
            }
            _ => CatalogError::Http { url: url.to_string(), source: Box::new(e) },
        })?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

/// `_cached_download` (`sidecar_dsp.py:114-124`): fetch a repo-relative file from
/// raw.githubusercontent, cached by path — the exact same cache-key scheme (`/`/`\`
/// folded to `__`) so it reads and writes the same on-disk cache files the sidecar's
/// own `_cached_download` does.
pub(crate) fn cached_download(rel_path: &str) -> Result<Vec<u8>, CatalogError> {
    let safe = rel_path.replace(['/', '\\'], "__");
    let cache_path = cache_dir()?.join("files").join(&safe);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if cache_path.exists() {
        return Ok(std::fs::read(&cache_path)?);
    }
    let url = format!("{GH_RAW}/{}", urlencode_path(rel_path));
    let data = http_get_binary(&url)?;
    std::fs::write(&cache_path, &data)?;
    Ok(data)
}

/// `FrequencyResponse.read_csv`'s own decode fallback (`frequency_response.py:108-113`):
/// UTF-8 first, then windows-1252 on failure — a handful of older measurement files use
/// it (curly quotes, degree signs typed in a legacy codepage) rather than UTF-8.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (text, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            text.into_owned()
        }
    }
}

/// Fetch and parse a measurement or target CSV by its repo-relative path — as returned
/// by the sidecar's `list_headphones`/`list_targets` catalogue (e.g.
/// `"measurements/oratory1990/data/over-ear/Sennheiser HD 600.csv"` or
/// `"targets/Harman over-ear 2018.csv"`). See the module doc for the boundary: this
/// crate fetches a *known* path, it doesn't browse the catalogue to find one.
pub fn fetch_curve(rel_path: &str) -> Result<(Vec<f64>, Vec<f64>), CatalogError> {
    let bytes = cached_download(rel_path)?;
    let text = decode_text(&bytes);
    let (frequency, raw) = parse_csv(&text)?;
    Ok(sort_by_frequency(frequency, raw))
}

/// `FrequencyResponse.__init__` sorts every column by frequency (`_sort`,
/// `frequency_response.py`) before any interpolation runs; `parse_csv` itself doesn't
/// (that's this function's job, not the parser's), and `cageq_peq_solver::grid`'s
/// interpolation binary-searches assuming ascending order.
fn sort_by_frequency(frequency: Vec<f64>, raw: Vec<f64>) -> (Vec<f64>, Vec<f64>) {
    let mut pairs: Vec<(f64, f64)> = frequency.into_iter().zip(raw).collect();
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    pairs.into_iter().unzip()
}

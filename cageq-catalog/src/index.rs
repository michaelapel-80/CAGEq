//! Port of `sidecar_dsp.py`'s `build_index`/`list_targets`/`_rig_map_for_source` — the
//! AutoEq measurement/target *catalogue* (browsing: "what paths exist", as opposed to
//! [`crate::fetch_curve`]'s "given a path, what's the curve"). GitHub's tree/contents
//! API plus each measurement source's own `name_index.tsv` for the `rig` field.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{cache_dir, cached_download, decode_text, http_get_binary, CatalogError};

const GH_API: &str = "https://api.github.com/repos/jaakkopasanen/AutoEq";
/// `ThreadPoolExecutor(max_workers=min(12, len(sources)))` (`sidecar_dsp.py:187`) — a
/// fixed worker count rather than one thread per source: dozens of sources means
/// dozens of threads all blocked on I/O otherwise, for no extra throughput once the
/// network itself is saturated.
const MAX_RIG_FETCH_WORKERS: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeadphoneEntry {
    pub source: String,
    pub form_factor: String,
    pub name: String,
    pub path: String,
    pub rig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TargetEntry {
    pub name: String,
    pub path: String,
}

fn http_get_json(url: &str) -> Result<Value, CatalogError> {
    let bytes = http_get_binary(url)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn cached_index<T: Serialize + for<'de> Deserialize<'de>>(cache_file: &str, refresh: bool, build: impl FnOnce() -> Result<T, CatalogError>) -> Result<T, CatalogError> {
    let path = cache_dir()?.join(cache_file);
    if !refresh
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str(&text)
    {
        return Ok(cached);
    }
    let value = build()?;
    std::fs::write(&path, serde_json::to_string(&value)?)?;
    Ok(value)
}

/// `_rig_map_for_source` (`sidecar_dsp.py:129-152`): `{(form_factor, name): rig}` for
/// one measurement source, parsed from its `name_index.tsv` (columns: url,
/// source_name, name, form, rig). Best-effort — an empty map on a missing file (a
/// source with no `name_index.tsv`, a 404) or any parse problem, exactly like the
/// Python version's blanket `except Exception: return {}`.
fn rig_map_for_source(source: &str) -> HashMap<(String, String), String> {
    let Ok(bytes) = cached_download(&format!("measurements/{source}/name_index.tsv")) else {
        return HashMap::new();
    };
    let text = decode_text(&bytes);
    let mut rigs = HashMap::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
        if cols.len() < 5 {
            continue;
        }
        let (name, form, rig) = (cols[2].trim(), cols[3].trim(), cols[4].trim());
        // First published entry wins (`setdefault`) — multiple raw rows map to the
        // same rig, consistent per model.
        if !name.is_empty() && form != "ignore" && !rig.is_empty() {
            rigs.entry((form.to_string(), name.to_string())).or_insert_with(|| rig.to_string());
        }
    }
    rigs
}

fn rig_maps_for(sources: &[String]) -> HashMap<String, HashMap<(String, String), String>> {
    if sources.is_empty() {
        return HashMap::new();
    }
    let worker_count = MAX_RIG_FETCH_WORKERS.min(sources.len());
    let chunk_size = sources.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let handles: Vec<_> = sources
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(|s| (s.clone(), rig_map_for_source(s))).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

/// `build_index` (`sidecar_dsp.py:155-195`): the headphone catalogue —
/// `[{source, form_factor, name, path, rig}]` — built from one recursive GitHub tree
/// call (~6800 entries), enriched with each source's rig, then cached to disk.
/// `refresh: false` reuses that cache unconditionally, matching the Python version
/// (which never checks the cache's age, only whether it exists).
pub fn build_index(refresh: bool) -> Result<Vec<HeadphoneEntry>, CatalogError> {
    cached_index("headphone_index_v2.json", refresh, || {
        let root = http_get_json(&format!("{GH_API}/git/trees/master"))?;
        let meas_sha = root["tree"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["path"] == "measurements")
            .and_then(|e| e["sha"].as_str())
            .ok_or_else(|| CatalogError::Json(serde_json::Error::io(std::io::Error::other("no 'measurements' entry in the repo tree"))))?
            .to_string();
        let tree = http_get_json(&format!("{GH_API}/git/trees/{meas_sha}?recursive=1"))?;
        if tree["truncated"].as_bool().unwrap_or(false) {
            return Err(CatalogError::TruncatedTree { sha: meas_sha });
        }

        let mut index: Vec<HeadphoneEntry> = Vec::new();
        for e in tree["tree"].as_array().into_iter().flatten() {
            if e["type"] != "blob" {
                continue;
            }
            let Some(path) = e["path"].as_str() else { continue };
            if !path.ends_with(".csv") {
                continue;
            }
            let parts: Vec<&str> = path.split('/').collect();
            // `<source>/data/<form-factor>/<model>.csv`
            if parts.len() >= 4 && parts[1] == "data" {
                index.push(HeadphoneEntry {
                    source: parts[0].to_string(),
                    form_factor: parts[2].to_string(),
                    name: parts[parts.len() - 1].trim_end_matches(".csv").to_string(),
                    path: format!("measurements/{path}"),
                    rig: String::new(),
                });
            }
        }

        let mut sources: Vec<String> = index.iter().map(|e| e.source.clone()).collect();
        sources.sort();
        sources.dedup();
        let rig_maps = rig_maps_for(&sources);
        for e in &mut index {
            if let Some(map) = rig_maps.get(&e.source)
                && let Some(rig) = map.get(&(e.form_factor.clone(), e.name.clone()))
            {
                e.rig = rig.clone();
            }
        }

        index.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.source.cmp(&b.source)));
        Ok(index)
    })
}

/// `list_targets` (`sidecar_dsp.py:198-213`): available target curves —
/// `[{name, path}]` — from the `targets/` directory listing.
pub fn list_targets(refresh: bool) -> Result<Vec<TargetEntry>, CatalogError> {
    cached_index("targets_index.json", refresh, || {
        let entries = http_get_json(&format!("{GH_API}/contents/targets"))?;
        let mut targets: Vec<TargetEntry> = entries
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "file" && e["name"].as_str().is_some_and(|n| n.ends_with(".csv")))
            .map(|e| {
                let name = e["name"].as_str().unwrap();
                TargetEntry { name: name.trim_end_matches(".csv").to_string(), path: format!("targets/{name}") }
            })
            .collect();
        targets.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(targets)
    })
}


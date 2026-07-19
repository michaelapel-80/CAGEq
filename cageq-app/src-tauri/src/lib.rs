use cageq_core::{DeviceConfig, Filter, FilterType};

/// First wiring of the backend into the app: returns a sample computed device
/// config, so the React <-> Rust <-> cageq-core path (and the serde contract on
/// DeviceConfig) is exercised end to end.
///
/// TODO: replace with a cageq-core-backed command that actually runs
/// `calculate_filters` and writes cageq.txt — that needs the sidecar spawn closure
/// and EqAPO's config dir wired into Tauri managed state.
#[tauri::command]
fn sample_config(device: String) -> DeviceConfig {
    DeviceConfig {
        device,
        preamp_db: -6.5,
        filters: vec![
            Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
            Filter { kind: FilterType::Peaking, freq_hz: 2500.0, gain_db: -2.4, q: 1.4 },
        ],
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![sample_config])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

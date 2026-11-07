//! GUI用のデバイス列挙と、音声設定だけの保存。収録エンジンには変更を加えない。
use crate::{audio_input::cpal_device_name, config::{AudioConfig, Config}};
use anyhow::{Context, Result, ensure};
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::{ffi::{c_char, CStr, CString}, hash::{Hash, Hasher}, io::Write, path::Path};

#[derive(Serialize)]
struct RateRange { min: u32, max: u32, channels: u16 }
#[derive(Serialize)]
struct Device { name: String, is_default: bool, ranges: Vec<RateRange> }
#[derive(Deserialize)]
struct Update { revision: String, device_id: String, output_device_id: String, sample_rate: u32 }

fn revision(text: &str) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

fn devices(input: bool) -> Result<Vec<Device>> {
    let host = cpal::default_host();
    let default = if input { host.default_input_device() } else { host.default_output_device() }
        .and_then(|device| cpal_device_name(&device).ok());
    let devices = if input { host.input_devices()? } else { host.output_devices()? };
    let mut found = Vec::new();
    for device in devices {
        let Ok(name) = cpal_device_name(&device) else { continue };
        let ranges: Vec<_> = if input {
            device.supported_input_configs().map(|configs| configs.collect())
        } else {
            device.supported_output_configs().map(|configs| configs.collect())
        }.unwrap_or_default();
        let ranges = ranges.into_iter().filter(|config| matches!(config.sample_format(),
            cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16 | cpal::SampleFormat::I32))
            .map(|config| RateRange { min: config.min_sample_rate(), max: config.max_sample_rate(), channels: config.channels() }).collect();
        found.push(Device { is_default: default.as_ref() == Some(&name), name, ranges });
    }
    Ok(found)
}

fn supported(devices: &[Device], name: &str, rate: u32) -> bool {
    devices.iter().any(|device| (if name == "default" { device.is_default } else { device.name == name })
        && device.ranges.iter().any(|range| range.min <= rate && rate <= range.max))
}

fn edit(text: &str, update: &Update) -> Result<String> {
    ensure!(revision(text) == update.revision, "設定ファイルが変更されています。再読み込みしてください");
    let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|_| anyhow::anyhow!("設定ファイルの形式が不正です"))?;
    if doc.get("audio").is_none() { doc["audio"] = toml_edit::Item::Table(toml_edit::Table::new()); }
    ensure!(doc["audio"].is_table(), "audio 設定は通常のテーブルで記述してください");
    for (key, value) in [("device_id", toml_edit::Value::from(update.device_id.clone())),
                         ("output_device_id", toml_edit::Value::from(update.output_device_id.clone())),
                         ("sample_rate", toml_edit::Value::from(i64::from(update.sample_rate)))] {
        let mut value = value;
        if let Some(old) = doc["audio"].get(key).and_then(|item| item.as_value()) { *value.decor_mut() = old.decor().clone(); }
        doc["audio"][key] = toml_edit::Item::Value(value);
    }
    let result = doc.to_string();
    let config: Config = toml::from_str(&result).map_err(|_| anyhow::anyhow!("設定内容を読み込めません"))?;
    config.validate()?;
    Ok(result)
}

fn save(path: &Path, original: &str, edited: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("保存先が不明です")?)?;
    file.as_file().set_permissions(std::fs::metadata(path)?.permissions())?;
    file.write_all(edited.as_bytes())?;
    file.as_file().sync_all()?;
    ensure!(std::fs::read_to_string(path)? == original, "設定ファイルが変更されています。再読み込みしてください");
    file.persist(path).map_err(|error| error.error).context("設定ファイルを保存できません")?;
    Ok(())
}

fn request(path: &Path, update: Option<Update>) -> Result<serde_json::Value> {
    let path = std::fs::canonicalize(path).context("設定ファイルが見つかりません")?;
    let original = std::fs::read_to_string(&path)?;
    let inputs = devices(true)?;
    let outputs = devices(false)?;
    let text = if let Some(update) = update {
        ensure!(supported(&inputs, &update.device_id, update.sample_rate), "選択した入力デバイスでこのサンプルレートは利用できません。デバイス一覧を更新してください");
        ensure!(supported(&outputs, &update.output_device_id, update.sample_rate), "選択した出力デバイスでこのサンプルレートは利用できません。デバイス一覧を更新してください");
        let edited = edit(&original, &update)?;
        save(&path, &original, &edited)?;
        edited
    } else { original };
    let config: Config = toml::from_str(&text).map_err(|_| anyhow::anyhow!("設定ファイルの形式が不正です"))?;
    let audio: AudioConfig = config.audio;
    Ok(serde_json::json!({"audio": audio, "revision": revision(&text), "inputs": inputs, "outputs": outputs}))
}

/// # Safety
/// path はNUL終端UTF-8。updateはNULL（読み込み）または更新JSON。戻り値はdcr_string_freeで解放する。
#[no_mangle]
pub unsafe extern "C" fn dcr_audio_settings(path: *const c_char, update: *const c_char) -> *mut c_char {
    let result = std::panic::catch_unwind(|| -> Result<serde_json::Value> {
        ensure!(!path.is_null(), "設定ファイルを選択してください");
        let path = CStr::from_ptr(path).to_str()?;
        let update = if update.is_null() { None } else {
            Some(serde_json::from_str(CStr::from_ptr(update).to_str()?).context("音声設定の更新内容が不正です")?)
        };
        request(Path::new(path), update)
    });
    let reply = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => serde_json::json!({"error": format!("{error:#}")}),
        Err(_) => serde_json::json!({"error": "音声設定の処理に失敗しました"}),
    };
    CString::new(reply.to_string()).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(text: &str) -> Update { Update { revision: revision(text), device_id: "MacBook Airのマイク".into(), output_device_id: "default".into(), sample_rate: 48000 } }
    #[test]
    fn editing_preserves_other_settings_comments_and_rejects_stale_changes() {
        let source = "# keep\n[audio]\ndevice_id = 'default' # microphone\nsample_rate = 16000\nchannels = 4\n\n[transcribe]\nregion = 'ap-northeast-1' # keep region\n";
        let edited = edit(source, &update(source)).unwrap();
        assert!(edited.contains("# microphone"));
        assert!(edited.contains("region = 'ap-northeast-1' # keep region"));
        let config: Config = toml::from_str(&edited).unwrap();
        assert_eq!(config.audio.channels, 4);
        assert_eq!(config.audio.sample_rate, 48000);
        assert_eq!(config.audio.device_id, "MacBook Airのマイク");
        assert!(edit(&edited, &update(source)).is_err());
    }
    #[test]
    fn save_rejects_external_edits_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "original").unwrap();
        save(&path, "original", "updated").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        assert!(save(&path, "original", "overwrite").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
    }
    #[test]
    fn rates_must_match_selected_device() {
        let devices = vec![Device { name: "mic".into(), is_default: true, ranges: vec![RateRange {min: 44100, max: 48000, channels: 1}] }];
        assert!(supported(&devices, "default", 48000));
        assert!(supported(&devices, "mic", 44100));
        assert!(!supported(&devices, "mic", 24000));
        assert!(!supported(&devices, "missing", 48000));
    }
}

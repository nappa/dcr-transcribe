//! SwiftUI から同一プロセス内のエンジンを操作する C ABI。
//! ポインタは start で取得し、destroy まで同じ所有者が直列に操作する。
use crate::{
    config::Config,
    tui_state::{TranscribeStatus, TuiState},
    types::VadState,
};
use std::{
    ffi::{c_char, CStr, CString},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Outcome {
    finished: bool,
    error: Option<String>,
    recordings: String,
    segments: String,
}

pub struct EmbeddedEngine {
    state: TuiState,
    running: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    outcome: Arc<Mutex<Outcome>>,
    worker: Option<JoinHandle<()>>,
}

fn resolve(base: &Path, path: &mut String) {
    if !path.is_empty() && !path.starts_with('~') && Path::new(path).is_relative() {
        *path = base.join(&*path).to_string_lossy().into_owned();
    }
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    let path = std::fs::canonicalize(path)?;
    let base = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("設定ファイルの場所を解決できません"))?;
    let mut config = Config::from_file(&path)?;
    resolve(base, &mut config.output.wav_output_dir);
    resolve(base, &mut config.segments.dir);
    resolve(base, &mut config.tunnel.bin_dir);
    resolve(base, &mut config.tunnel.config);
    resolve(base, &mut config.tunnel.token_file);
    resolve(base, &mut config.uplink.ca_file);
    resolve(base, &mut config.uplink.cert_file);
    resolve(base, &mut config.uplink.key_file);
    if config.tunnel.cloudflared.contains('/') {
        resolve(base, &mut config.tunnel.cloudflared);
    }
    if let Some(remote) = &mut config.remote {
        resolve(base, &mut remote.login_key_file);
    }
    Ok(config)
}

impl EmbeddedEngine {
    fn launch(path: PathBuf) -> anyhow::Result<Self> {
        let state = TuiState::new();
        let running = Arc::new(AtomicBool::new(true));
        let ready = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let worker_state = state.clone();
        let worker_running = running.clone();
        let worker_ready = ready.clone();
        let worker_outcome = outcome.clone();
        let worker = std::thread::Builder::new()
            .name("dcr-engine".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> anyhow::Result<()> {
                        let config = load_config(&path)?;
                        {
                            let mut result =
                                worker_outcome.lock().unwrap_or_else(|e| e.into_inner());
                            result.recordings = config.output.wav_output_dir.clone();
                            result.segments = config.segments.dir.clone();
                        }
                        // GUI のログも設定ファイルの隣へ。設定内容や認証情報はログへ出さない。
                        if let Some(base) = path.parent() {
                            if let Ok(file) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(base.join("dcr-transcribe.log"))
                            {
                                let _ = env_logger::Builder::from_env(
                                    env_logger::Env::default().default_filter_or("info"),
                                )
                                .format_timestamp_millis()
                                .filter_module("flacenc", log::LevelFilter::Off)
                                .target(env_logger::Target::Pipe(Box::new(file)))
                                .try_init();
                            }
                        }
                        let runtime = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()?;
                        // 録音処理、任意の閲覧用HTTPサーバーとも、このプロセスのランタイムで動く。
                        let result = runtime.block_on(crate::engine::run(
                            config,
                            worker_state,
                            worker_running.clone(),
                            worker_ready.clone(),
                            false,
                        ));
                        runtime.shutdown_timeout(std::time::Duration::from_secs(5));
                        result
                    },
                ));
                worker_ready.store(false, Ordering::SeqCst);
                worker_running.store(false, Ordering::SeqCst);
                let mut outcome = worker_outcome.lock().unwrap_or_else(|e| e.into_inner());
                outcome.finished = true;
                outcome.error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("収録処理が内部エラーで終了しました".into()),
                };
            })?;
        Ok(Self {
            state,
            running,
            ready,
            outcome,
            worker: Some(worker),
        })
    }

    fn snapshot(&self) -> serde_json::Value {
        let outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        let phase = if outcome.finished {
            if outcome.error.is_some() {
                "failed"
            } else {
                "stopped"
            }
        } else if !self.running.load(Ordering::SeqCst) {
            "stopping"
        } else if self.ready.load(Ordering::SeqCst) {
            "running"
        } else {
            "starting"
        };
        let channels: Vec<_> = self.state.get_all_channels().into_iter().map(|channel| {
            let mut entries: Vec<_> = channel.transcripts.iter().map(|entry| serde_json::json!({
                "time": entry.time, "seconds": entry.seconds, "text": entry.text, "partial": false
            })).collect();
            if let Some(entry) = channel.partial_transcript {
                entries.push(serde_json::json!({"time": entry.time, "seconds": entry.seconds, "text": entry.text, "partial": true}));
            }
            serde_json::json!({
                "id": channel.channel_id, "name": channel.channel_name,
                "level_db": channel.current_volume_db, "threshold_db": channel.vad_threshold_db,
                "voice": matches!(channel.vad_state, VadState::Voice { .. }),
                "transcribe": match channel.transcribe_status { TranscribeStatus::Disconnected => 0, TranscribeStatus::Connected => 1, TranscribeStatus::Error => 2 },
                "quality_warning": channel.input_quality_warning, "recording_error": channel.recording_error,
                "transcripts": entries
            })
        }).collect();
        let mobile_url = self.state.share_info.lock().unwrap_or_else(|e| e.into_inner())
            .as_ref().and_then(|share| share.public.clone());
        let uplink = self.state.uplink_status.lock().unwrap_or_else(|e| e.into_inner()).clone().map(|(health, text)| {
            let health = match health {
                crate::tui_state::Health::Good => "good",
                crate::tui_state::Health::Warn => "warn",
                crate::tui_state::Health::Bad => "bad",
                crate::tui_state::Health::Unknown => "unknown",
            };
            serde_json::json!({ "health": health, "text": text })
        });
        serde_json::json!({
            "mobile_url": mobile_url,
            "uplink": uplink,
            "clock_offset_ms": self.state.clock.offset_ms(),
            "version": 1, "pid": std::process::id(), "phase": phase, "error": outcome.error,
            "recordings_dir": outcome.recordings, "segments_dir": outcome.segments,
            "input_problem": self.state.input_health.problem(), "output_problem": self.state.output_health.problem(),
            "selected_output": self.state.get_selected_channel_for_output(),
            "selected_outputs": self.state.get_selected_channels_for_output(), "channels": channels
        })
    }
}

impl Drop for EmbeddedEngine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[no_mangle]
pub extern "C" fn dcr_engine_abi_version() -> u32 {
    1
}

/// # Safety
/// path は NUL 終端 UTF-8 文字列。戻り値の所有権を取得し、最後に destroy を一度だけ呼ぶ。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_start(path: *const c_char) -> *mut EmbeddedEngine {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    std::panic::catch_unwind(|| {
        let Ok(path) = CStr::from_ptr(path).to_str() else {
            return std::ptr::null_mut();
        };
        if ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return std::ptr::null_mut();
        }
        match EmbeddedEngine::launch(PathBuf::from(path)) {
            Ok(engine) => Box::into_raw(Box::new(engine)),
            Err(_) => {
                ACTIVE.store(false, Ordering::SeqCst);
                std::ptr::null_mut()
            }
        }
    })
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// engine は有効なハンドル。返した文字列は dcr_string_free で解放する。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_snapshot(engine: *const EmbeddedEngine) -> *mut c_char {
    if engine.is_null() {
        return std::ptr::null_mut();
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CString::new((*engine).snapshot().to_string())
            .map(CString::into_raw)
            .unwrap_or(std::ptr::null_mut())
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// engine は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_stop(engine: *const EmbeddedEngine) {
    if let Some(engine) = engine.as_ref() {
        engine.running.store(false, Ordering::SeqCst);
    }
}

/// # Safety
/// engine は有効なハンドル。channel=-1 でモニター解除。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_select_output(engine: *const EmbeddedEngine, channel: i32) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(engine) = engine.as_ref() {
            let id = usize::try_from(channel)
                .ok()
                .filter(|id| engine.state.get_channel(*id).is_some());
            engine.state.set_selected_channel_for_output(id);
        }
    }));
}

/// # Safety
/// engine は有効なハンドル。指定チャンネルだけをオン・オフする。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_output_enabled(engine: *const EmbeddedEngine, channel: i32, enabled: bool) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(engine) = engine.as_ref() {
            if let Ok(channel) = usize::try_from(channel) {
                if engine.state.get_channel(channel).is_some() { engine.state.set_output_enabled(channel, enabled); }
            }
        }
    }));
}

/// # Safety
/// engine は有効なハンドル。プレイバック中も選択状態は保持し、出力だけミュートする。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_mute_output(engine: *const EmbeddedEngine, muted: bool) {
    if let Some(engine) = engine.as_ref() {
        engine.state.output_muted.store(muted, Ordering::SeqCst);
    }
}

/// # Safety
/// start で取得したハンドルを一度だけ解放する。停止・WAV確定を待つためUIスレッドでは呼ばない。
#[no_mangle]
pub unsafe extern "C" fn dcr_engine_destroy(engine: *mut EmbeddedEngine) {
    if !engine.is_null() {
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(Box::from_raw(engine))));
    }
}

/// # Safety
/// snapshot が返した文字列を一度だけ解放する。
#[no_mangle]
pub unsafe extern "C" fn dcr_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(CString::from_raw(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_config_is_reported_without_audio_or_network_and_can_restart() {
        for _ in 0..2 {
            let path = CString::new("/nonexistent/dcr-engine-test.toml").unwrap();
            unsafe {
                let handle = dcr_engine_start(path.as_ptr());
                assert!(!handle.is_null());
                let second = dcr_engine_start(path.as_ptr());
                assert!(second.is_null(), "同一プロセス内の二重起動を防ぐ");
                let mut snapshot = serde_json::Value::Null;
                for _ in 0..100 {
                    let text = dcr_engine_snapshot(handle);
                    assert!(!text.is_null());
                    snapshot = serde_json::from_slice(CStr::from_ptr(text).to_bytes()).unwrap();
                    dcr_string_free(text);
                    if snapshot["phase"] == "failed" {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert_eq!(snapshot["phase"], "failed");
                assert_eq!(snapshot["pid"], std::process::id());
                assert!(snapshot["error"].is_string());
                dcr_engine_stop(handle);
                dcr_engine_destroy(handle);
            }
        }
    }
    #[test]
    fn null_handles_are_safe() {
        unsafe {
            assert!(dcr_engine_start(std::ptr::null()).is_null());
            assert!(dcr_engine_snapshot(std::ptr::null()).is_null());
            dcr_engine_stop(std::ptr::null());
            dcr_engine_destroy(std::ptr::null_mut());
            dcr_string_free(std::ptr::null_mut());
        }
    }
    #[test]
    fn paths_resolve_without_changing_process_working_directory() {
        let mut path = "recordings".to_string();
        resolve(Path::new("/tmp/session"), &mut path);
        assert_eq!(path, "/tmp/session/recordings");
        let mut command = "~/.cloudflared/config.yml".into();
        resolve(Path::new("/tmp/session"), &mut command);
        assert_eq!(command, "~/.cloudflared/config.yml");
    }

    #[test]
    fn gui_uplink_certificates_resolve_relative_to_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[uplink]\nenabled = true\nserver = '127.0.0.1:443'\nca_file = 'tls/ca.pem'\ncert_file = 'tls/venue.pem'\nkey_file = 'tls/venue.key'\n").unwrap();
        let config = load_config(&path).unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(Path::new(&config.uplink.ca_file), base.join("tls/ca.pem"));
        assert_eq!(Path::new(&config.uplink.cert_file), base.join("tls/venue.pem"));
        assert_eq!(Path::new(&config.uplink.key_file), base.join("tls/venue.key"));
    }
}

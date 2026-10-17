mod aac;
mod access;
mod audio_input;
mod audio_output;
mod aws_transcribe;
mod buffer;
mod channel_processor;
mod config;
mod live;
mod flac_encoder;
mod opus_encoder;
mod remote_server;
mod segment_recorder;
mod transcribe;
mod transcribe_backend;
mod tui;
mod tui_state;
mod types;
mod vad;
mod wav_writer;
mod whisper_api;

use anyhow::{Context, Result};
use audio_input::AudioInput;
use audio_output::AudioOutput;
use aws_credential_types::provider::ProvideCredentials;
use channel_processor::ChannelProcessor;
use config::{Config, TranscribeBackendType};
use env_logger::Env;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use tui::TuiApp;
use tui_state::TuiState;

/// AWS 認証情報を起動時に検証する。
///
/// SDK の標準クレデンシャルチェーン(env vars → ~/.aws/credentials → IMDS など)を
/// 試して、いずれかから取得できない場合は明確なエラーで終了する。
/// これにより「起動はするが文字起こしが永遠に失敗する」状況を回避する。
async fn validate_aws_credentials() -> Result<()> {
    log::info!("AWS 認証情報を確認中...");
    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let provider = sdk_config
        .credentials_provider()
        .ok_or_else(|| anyhow::anyhow!("AWS credentials provider が見つかりません"))?;

    let creds = provider.provide_credentials().await.map_err(|e| {
        anyhow::anyhow!(
            "AWS 認証情報の読み込みに失敗しました: {}\n\n\
             以下のいずれかを設定してから再起動してください:\n\
             - 環境変数 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION を export\n\
             - ~/.aws/credentials ファイルに [default] プロファイルを設定\n\
             - その他、AWS SDK が標準で参照するクレデンシャルソース",
            e
        )
    })?;

    let key_id = creds.access_key_id();
    let masked = if key_id.len() > 4 {
        format!("...{}", &key_id[key_id.len() - 4..])
    } else {
        "****".to_string()
    };
    log::info!("AWS 認証情報を確認(access_key_id={})", masked);
    Ok(())
}

/// ログファイルに書き込むためのWriter
struct LogWriter(Arc<Mutex<std::fs::File>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // ログファイルを開く
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("dcr-transcribe.log")
        .context("ログファイルを開けませんでした")?;

    let log_writer = LogWriter(Arc::new(Mutex::new(log_file)));

    // ロガーを初期化（ファイルに出力）
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .filter_module("flacenc", log::LevelFilter::Off)
        .target(env_logger::Target::Pipe(Box::new(log_writer)))
        .init();

    // コマンドライン引数をパース
    let args: Vec<String> = std::env::args().collect();

    // デバイス一覧表示モード
    if args.len() > 1 && args[1] == "--show-interfaces" {
        println!("=== 入力デバイス ===");
        AudioInput::list_devices()?;
        println!();
        println!("=== 出力デバイス ===");
        AudioOutput::list_devices()?;
        return Ok(());
    }

    // 設定ファイル生成モード
    if args.len() > 1 && args[1] == "--generate-config" {
        let config_path = if args.len() > 2 {
            &args[2]
        } else {
            "config.toml"
        };
        Config::write_default(config_path)?;
        println!("設定ファイルを生成しました: {}", config_path);
        return Ok(());
    }

    // 設定ファイルのパス
    let config_path = if args.len() > 1 && !args[1].starts_with("--") {
        &args[1]
    } else {
        "config.toml"
    };

    // 設定を読み込み
    let config = Config::load_or_default(config_path)?;

    log::info!("dcr-transcribe を起動します");
    log::info!("設定: {:?}", config);

    // AWS バックエンド使用時は、起動前に認証情報が解決できるかを確認する。
    // 解決できないまま起動するとログだけ汚れて文字起こしが永遠に動かないので、fail-fast。
    if config.transcribe.backend == TranscribeBackendType::Aws {
        validate_aws_credentials()
            .await
            .context("AWS 認証情報の検証に失敗しました")?;
    }

    // 全チャンネル共通の start_time を作成(以降で使用)
    let start_time = std::time::SystemTime::now();

    // ライブ再生(Remote サーバが有効なときだけ)
    let remote_enabled = config.remote.as_ref().is_some_and(|r| r.enabled);
    let live_hub = (remote_enabled && config.live.enabled).then(|| {
        live::LiveHub::new(
            config.audio.sample_rate,
            config.live.bitrate,
            config.live.profile,
            config.channels.iter().filter(|c| c.enabled).map(|c| c.id).collect(),
        )
    });

    // Remote サーバ(SSE)を起動
    let mut live_hub_for_input = None;
    let remote_state: Option<Arc<remote_server::RemoteState>> = match config.remote.clone() {
        Some(cfg) if cfg.enabled => match remote_server::start(
            cfg,
            &config.channels,
            start_time,
            config.segments.enabled,
            live_hub.clone(),
        )
        .await
        {
            Ok(state) => {
                live_hub_for_input = live_hub;
                Some(state)
            }
            Err(e) => {
                log::error!("Remote サーバ起動失敗(無効化して続行): {}", e);
                None
            }
        },
        _ => None,
    };

    // Ctrl+C ハンドラを設定
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        log::info!("停止シグナルを受信しました...");
        running_clone.store(false, Ordering::SeqCst);
    })?;

    // TUI状態を作成
    let tui_state = TuiState::new();

    // 区間録音は起動ごとに日時名のディレクトリへまとめる
    let segment_dir = std::path::Path::new(&config.segments.dir).join(
        chrono::DateTime::<chrono::Local>::from(start_time)
            .format("%Y%m%d_%H%M%S")
            .to_string(),
    );
    if config.segments.enabled {
        log::info!("区間録音: {:?}", segment_dir);
    }

    // チャンネルプロセッサを作成
    let mut processors = Vec::new();
    // デバイスのチャンネル番号 (= channel_config.id) で引けるように並べる。無効チャンネルは None
    let mut channel_senders: Vec<Option<mpsc::Sender<_>>> =
        (0..config.audio.channels).map(|_| None).collect();

    for channel_config in &config.channels {
        if !channel_config.enabled {
            log::info!("チャンネル {} は無効です", channel_config.id);
            continue;
        }
        anyhow::ensure!(
            channel_config.id < channel_senders.len(),
            "チャンネル {} ({}) の id がデバイスのチャンネル数 ({}) を超えています",
            channel_config.id,
            channel_config.name,
            config.audio.channels
        );

        // TUI状態にチャンネルを追加
        tui_state.add_channel(channel_config.id, channel_config.name.clone());

        let (tx, rx) = mpsc::channel(128);
        channel_senders[channel_config.id] = Some(tx);

        let mut processor = ChannelProcessor::new(
            channel_config,
            &config.vad,
            &config.buffer,
            &config.transcribe,
            config.whisper.as_ref(),
            &config.output,
            config.audio.sample_rate,
            start_time,
        )
        .await
        .with_context(|| {
            format!(
                "チャンネル {} ({}) の初期化に失敗",
                channel_config.id, channel_config.name
            )
        })?;

        // TUI状態を設定
        processor.set_tui_state(tui_state.clone());

        if config.segments.enabled {
            let recorder = segment_recorder::SegmentRecorder::new(
                channel_config.id,
                &segment_dir,
                config.audio.sample_rate,
                config.vad.threshold_db,
                &config.segments,
            )?;
            processor.set_segment_recorder(recorder);
        }

        processors.push((rx, processor));
    }

    // 各チャンネルプロセッサを開始
    for (_, processor) in &mut processors {
        processor.start().await?;
    }

    // AudioInputを作成して開始
    let mut audio_input = AudioInput::new(&config.audio)?;
    if let Some(hub) = live_hub_for_input {
        // 1 要素 = 1 コールバック(4096 フレーム ≒ 85 ms)。詰まったら入力側で捨てる
        let (live_tx, live_rx) = mpsc::channel(64);
        audio_input.set_live_tap(live_tx, hub.active_flag());
        tokio::spawn(hub.run(live_rx));
        log::info!("ライブ再生: 有効 ({:?}, {} bps)", config.live.profile, config.live.bitrate);
    }
    audio_input.start(channel_senders)?;

    // AudioOutputを作成して開始
    let output_device = if config.audio.output_device_id == "default" {
        None
    } else {
        Some(config.audio.output_device_id.as_str())
    };
    let mut audio_output = AudioOutput::new(output_device, config.audio.sample_rate)?;
    let audio_output_tx = audio_output.start()?;

    log::info!("録音を開始しました (Ctrl+C または 'q' で停止)");

    // TUIタスクを起動
    let tui_state_clone = tui_state.clone();
    let running_clone = running.clone();
    let tui_task = tokio::spawn(async move {
        let mut tui_app = TuiApp::new(tui_state_clone, running_clone);
        if let Err(e) = tui_app.run().await {
            log::error!("TUIエラー: {}", e);
        }
    });

    // 各チャンネルの処理タスクを起動
    let mut tasks = Vec::new();

    // プロセッサをマップに格納（channel_id -> processor）
    let processors_map = Arc::new(tokio::sync::Mutex::new(
        std::collections::HashMap::<usize, Arc<tokio::sync::Mutex<ChannelProcessor>>>::new(),
    ));

    for (mut rx, processor) in processors {
        let channel_id = processor.channel_id();

        // processorを共有するためにArcでラップ
        let processor = Arc::new(tokio::sync::Mutex::new(processor));

        // マップに登録
        {
            let mut map = processors_map.lock().await;
            map.insert(channel_id, processor.clone());
        }

        // タスク1: 音声チャンク処理スレッド
        let processor_clone = processor.clone();
        let running_clone = running.clone();
        let chunk_task = tokio::spawn(async move {
            use std::time::Instant;
            while running_clone.load(Ordering::SeqCst) {
                tokio::select! {
                    Some(chunk) = rx.recv() => {
                        let lock_start = Instant::now();
                        let mut proc = processor_clone.lock().await;
                        let lock_elapsed = lock_start.elapsed();

                        if lock_elapsed.as_millis() >= 10 {
                            log::warn!(
                                "チャンネル {}: ロック取得に {}ms（閾値10ms超過）",
                                proc.channel_id(),
                                lock_elapsed.as_millis()
                            );
                        }

                        if let Err(e) = proc.process_chunk(chunk).await {
                            log::error!("チャンク処理エラー: {}", e);
                        }
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                        // タイムアウト: ループを継続して running をチェック
                    }
                }
            }
        });
        tasks.push(chunk_task);

        // タスク2: 文字起こし結果取得スレッド
        let processor_clone = processor.clone();
        let running_clone = running.clone();
        let remote_state_clone = remote_state.clone();
        let transcript_task = tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                let mut proc = processor_clone.lock().await;
                let channel_id = proc.channel_id();

                // 録音を終えた区間を Remote サーバへ
                for segment in proc.take_finished_segments() {
                    if let Some(state) = &remote_state_clone {
                        state.push_segment(&segment);
                    }
                }

                // 文字起こし結果をポーリング
                let results = proc.poll_transcripts().await;
                // 結果は受信時点で録音中(または直前)の区間にひも付ける
                let segment_id = proc.last_segment_id();
                if !results.is_empty() {
                    log::debug!("チャンネル {}: 文字起こし結果取得 {} 件", channel_id, results.len());
                    for mut result in results {
                        // TUI状態に追加（フィラーワード削除は内部で実行）
                        proc.add_transcript_to_tui(&result);

                        // 途中状態でなく、かつフィラーワード削除後に内容がある場合のみログ出力
                        if !result.is_partial {
                            let cleaned_text = ChannelProcessor::remove_filler_words(&result.text);
                            if !cleaned_text.is_empty() && !ChannelProcessor::is_punctuation_only(&cleaned_text) {
                                // クリーニング後のテキストでログ出力
                                result.text = cleaned_text;
                                if let Ok(json) = serde_json::to_string(&result) {
                                    log::info!("{}", json);
                                }

                                // Remote サーバへ転送(同期、ブロックしない)
                                if let Some(state) = &remote_state_clone {
                                    state.push_with_segment(&result, segment_id);
                                }
                            }
                        } else if let Some(state) = &remote_state_clone {
                            // partial も(設定で有効な場合のみ内部で振り分け)
                            state.push_with_segment(&result, segment_id);
                        }
                    }
                }
            }

            // 停止処理
            let mut proc = processor_clone.lock().await;
            if let Err(e) = proc.stop().await {
                log::error!("プロセッサ停止エラー: {}", e);
            }
        });
        tasks.push(transcript_task);
    }

    // タスク3: 選択チャンネルを監視して音声出力を切り替え
    let processors_map_clone = processors_map.clone();
    let tui_state_clone = tui_state.clone();
    let running_clone = running.clone();
    let audio_output_tx_clone = audio_output_tx.clone();
    let output_monitor_task = tokio::spawn(async move {
        let mut last_selected: Option<usize> = None;

        while running_clone.load(Ordering::SeqCst) {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            let current_selected = tui_state_clone.get_selected_channel_for_output();

            // 選択が変更された場合
            if current_selected != last_selected {
                log::info!("音声出力チャンネル変更: {:?} -> {:?}", last_selected, current_selected);

                let map = processors_map_clone.lock().await;

                // 前のチャンネルから音声出力を解除
                if let Some(old_id) = last_selected {
                    if let Some(processor) = map.get(&old_id) {
                        let mut proc = processor.lock().await;
                        proc.clear_audio_output();
                    }
                }

                // 新しいチャンネルに音声出力を設定
                if let Some(new_id) = current_selected {
                    if let Some(processor) = map.get(&new_id) {
                        let mut proc = processor.lock().await;
                        proc.set_audio_output(audio_output_tx_clone.clone());
                    }
                }

                last_selected = current_selected;
            }
        }
    });
    tasks.push(output_monitor_task);

    // タスク4: チャンネルの状態(受信中・音量・Transcribe 接続)を Remote サーバへ。
    // 配信は変化があったときだけ(RemoteState::push_status 側で間引く)
    if let Some(state) = remote_state.clone() {
        let tui_state_clone = tui_state.clone();
        let running_clone = running.clone();
        tasks.push(tokio::spawn(async move {
            // 閾値をまたいで細かくばたつかないよう、最後に閾値を超えてからしばらくは受信中とみなす
            const RECEIVING_HOLD: std::time::Duration = std::time::Duration::from_millis(500);
            let mut last_voice = std::collections::HashMap::<usize, std::time::Instant>::new();
            while running_clone.load(Ordering::SeqCst) {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                let now = std::time::Instant::now();
                let items: Vec<remote_server::ChannelStatus> = tui_state_clone
                    .get_all_channels()
                    .iter()
                    .map(|ch| {
                        if ch.current_volume_db > ch.vad_threshold_db {
                            last_voice.insert(ch.channel_id, now);
                        }
                        remote_server::ChannelStatus {
                            channel: ch.channel_id,
                            level_db: ch.current_volume_db,
                            receiving: last_voice
                                .get(&ch.channel_id)
                                .is_some_and(|t| now.duration_since(*t) < RECEIVING_HOLD),
                            transcribe: match ch.transcribe_status {
                                tui_state::TranscribeStatus::Disconnected => 0,
                                tui_state::TranscribeStatus::Connected => 1,
                                tui_state::TranscribeStatus::Error => 2,
                            },
                        }
                    })
                    .collect();
                state.push_status(&items);
            }
        }));
    }

    // メインループ: 停止を待つ
    while running.load(Ordering::SeqCst) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // クリーンアップ
    log::info!("停止処理を開始します...");

    audio_input.stop();
    audio_output.stop();

    // TUIタスクの完了を待つ
    let _ = tui_task.await;

    // 他のタスクの完了を待つ
    for task in tasks {
        let _ = task.await;
    }

    log::info!("dcr-transcribe を終了しました");

    Ok(())
}

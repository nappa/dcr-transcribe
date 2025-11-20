mod audio_input;
mod audio_output;
mod aws_transcribe;
mod buffer;
mod channel_processor;
mod config;
mod flac_encoder;
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
use channel_processor::ChannelProcessor;
use config::Config;
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

#[tokio::main]
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

    // Ctrl+C ハンドラを設定
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        log::info!("停止シグナルを受信しました...");
        running_clone.store(false, Ordering::SeqCst);
    })?;

    // TUI状態を作成
    let tui_state = TuiState::new();

    // チャンネルプロセッサを作成
    let mut processors = Vec::new();
    let mut channel_senders = Vec::new();

    for channel_config in &config.channels {
        if !channel_config.enabled {
            log::info!("チャンネル {} は無効です", channel_config.id);
            continue;
        }

        // TUI状態にチャンネルを追加
        tui_state.add_channel(channel_config.id, channel_config.name.clone());

        let (tx, rx) = mpsc::channel(1024 * 1024);
        channel_senders.push(tx);

        let mut processor = ChannelProcessor::new(
            channel_config,
            &config.vad,
            &config.buffer,
            &config.transcribe,
            config.whisper.as_ref(),
            &config.output,
            config.audio.sample_rate,
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

        processors.push((rx, processor));
    }

    // 各チャンネルプロセッサを開始
    for (_, processor) in &mut processors {
        processor.start().await?;
    }

    // AudioInputを作成して開始
    let mut audio_input = AudioInput::new(&config.audio)?;
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
            while running_clone.load(Ordering::SeqCst) {
                tokio::select! {
                    Some(chunk) = rx.recv() => {
                        let mut proc = processor_clone.lock().await;
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
        let transcript_task = tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

                let mut proc = processor_clone.lock().await;
                let channel_id = proc.channel_id();

                // 文字起こし結果をポーリング
                let results = proc.poll_transcripts().await;
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
                            }
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

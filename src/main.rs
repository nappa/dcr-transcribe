mod audio_input;
mod buffer;
mod channel_processor;
mod config;
mod flac_encoder;
mod transcribe;
mod types;
mod vad;
mod wav_writer;

use anyhow::{Context, Result};
use audio_input::AudioInput;
use channel_processor::ChannelProcessor;
use config::Config;
use env_logger::Env;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    // ロガーを初期化
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .filter_module("flacenc", log::LevelFilter::Off)
        .init();

    // コマンドライン引数をパース
    let args: Vec<String> = std::env::args().collect();

    // デバイス一覧表示モード
    if args.len() > 1 && args[1] == "--show-interfaces" {
        AudioInput::list_devices()?;
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

    // チャンネルプロセッサを作成
    let mut processors = Vec::new();
    let mut channel_senders = Vec::new();

    for channel_config in &config.channels {
        if !channel_config.enabled {
            log::info!("チャンネル {} は無効です", channel_config.id);
            continue;
        }

        let (tx, rx) = mpsc::channel(1024 * 1024);
        channel_senders.push(tx);

        let processor = ChannelProcessor::new(
            channel_config,
            &config.vad,
            &config.buffer,
            &config.transcribe,
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

        processors.push((rx, processor));
    }

    // 各チャンネルプロセッサを開始
    for (_, processor) in &mut processors {
        processor.start().await?;
    }

    // AudioInputを作成して開始
    let mut audio_input = AudioInput::new(&config.audio)?;
    audio_input.start(channel_senders)?;

    log::info!("録音を開始しました (Ctrl+C で停止)");

    // 各チャンネルの処理タスクを起動
    let mut tasks = Vec::new();

    for (mut rx, processor) in processors {
        // processorを共有するためにArcでラップ
        let processor = Arc::new(tokio::sync::Mutex::new(processor));

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
                    for result in results {
                        // JSON形式で出力
                        if let Ok(json) = serde_json::to_string(&result) {
                            println!("{}", json);
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

    // メインループ: 停止を待つ
    while running.load(Ordering::SeqCst) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // クリーンアップ
    log::info!("停止処理を開始します...");

    audio_input.stop();

    // タスクの完了を待つ
    for task in tasks {
        let _ = task.await;
    }

    log::info!("dcr-transcribe を終了しました");

    Ok(())
}

use crate::{
    audio_input::AudioInput,
    audio_output::AudioOutput,
    channel_processor::ChannelProcessor,
    config::{Config, TranscribeBackendType},
    live, remote_server, segment_recorder, share,
    tui::TuiApp,
    tui_state::{self, TuiState},
    tunnel, uplink,
};
use anyhow::{Context, Result};
use aws_credential_types::provider::ProvideCredentials;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::mpsc;

async fn validate_aws_credentials(config: &crate::config::TranscribeConfig) -> Result<()> {
    log::info!("AWS 認証情報を確認中...");
    let sdk_config = crate::aws_settings::load(config).await?;
    let provider = sdk_config
        .credentials_provider()
        .ok_or_else(|| anyhow::anyhow!("AWS credentials provider が見つかりません"))?;

    provider.provide_credentials().await.map_err(|e| {
        anyhow::anyhow!(
            "AWS 認証情報の読み込みに失敗しました: {}\n\n\
             以下のいずれかを設定してから再起動してください:\n\
             - config.toml の [transcribe.aws] にアクセスキー、または profile を設定\n\
             - aws login を使うプロファイルは aws login で再認証\n\
             - 環境変数 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION を export\n\
             - ~/.aws/credentials ファイルに [default] プロファイルを設定\n\
             - その他、AWS SDK が標準で参照するクレデンシャルソース",
            e
        )
    })?;

    log::info!("AWS 認証情報を確認しました");
    Ok(())
}

/// TUI と SwiftUI が共有する収録・文字起こし処理。GUI 用のネットワーク接続は不要。
pub async fn run(
    config: Config,
    tui_state: TuiState,
    running: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    show_tui: bool,
) -> Result<()> {
    log::info!("dcr-transcribe を起動します(メモリアロケータ: {})", crate::allocator_desc());
    config.validate()?;

    // AWS バックエンド使用時は、起動前に認証情報が解決できるかを確認する。
    // 解決できないまま起動するとログだけ汚れて文字起こしが永遠に動かないので、fail-fast。
    if config.transcribe.backend == TranscribeBackendType::Aws {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            validate_aws_credentials(&config.transcribe),
        )
        .await
        .context("AWS 認証情報の検証がタイムアウトしました")?
        .context("AWS 認証情報の検証に失敗しました")?;
    }

    // 全チャンネル共通の start_time を作成(以降で使用)
    let start_time = std::time::SystemTime::now();
    // 時計のずれの監視(録音・文字起こしの時刻がこの機械の時計に依存するため)
    tokio::spawn(tui_state.clock.clone().run(config.clock.clone(), running.clone()));

    // ライブ再生(Remote サーバが有効なときだけ)
    let remote_enabled = config.remote.as_ref().is_some_and(|r| r.enabled);
    let live_hub = (remote_enabled && config.live.enabled).then(|| {
        live::LiveHub::new(
            config.audio.sample_rate,
            config.live.bitrate,
            config.live.profile,
            config
                .channels
                .iter()
                .filter(|c| c.enabled)
                .map(|c| c.id)
                .collect(),
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
            Some(tui_state.clock.clone()),
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

    // 中継サーバへのアップリンク(Remote サーバとは独立に使える)
    let (uplink_stop_tx, uplink_stop_rx) = tokio::sync::watch::channel(false);
    let mut uplink_live_tx = None;
    let (uplink, uplink_task) = if config.uplink.enabled {
        let up = uplink::Uplink::new(&config.uplink, &config.channels, start_time, config.audio.sample_rate)
            .context("アップリンクの設定が不正です")?;
        let (live_tx, live_rx) = uplink::live_queue();
        if up.live_enabled() {
            uplink_live_tx = Some(live_tx);
        }
        log::info!("アップリンク: {} へ QUIC で送ります", config.uplink.server);
        let task = tokio::spawn(up.clone().run(live_rx, uplink_stop_rx));
        (Some(up), Some(task))
    } else {
        (None, None)
    };

    // cloudflared(Remote サーバが立ち上がってから。サーバ無しでは公開するものが無い)
    let tunnel = if config.tunnel.enabled {
        if remote_state.is_some() {
            Some(tunnel::Tunnel::start(&config.tunnel).context("トンネルの設定が不正です")?)
        } else {
            log::warn!("トンネル: Remote サーバが無効なので cloudflared は起動しません");
            None
        }
    } else {
        None
    };

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

    if !running.load(Ordering::SeqCst) {
        for (_, processor) in &mut processors {
            processor.stop().await?;
        }
        if let Some(t) = tunnel {
            t.stop().await;
        }
        let _ = uplink_stop_tx.send(true);
        if let Some(task) = uplink_task {
            let _ = task.await;
        }
        return Ok(());
    }

    // AudioInputを作成して開始
    let mut audio_input = AudioInput::new(&config.audio)?;
    audio_input.set_health(tui_state.input_health.clone());
    if let Some(hub) = live_hub_for_input {
        // 1 要素 = 1 コールバック(1024 フレーム ≒ 21 ms)。詰まったら入力側で捨てる
        let (live_tx, live_rx) = mpsc::channel(256);
        audio_input.add_live_tap(live_tx, hub.active_flag());
        tokio::spawn(hub.run(live_rx));
        log::info!(
            "ライブ再生: 有効 ({:?}, {} bps)",
            config.live.profile,
            config.live.bitrate
        );
    }
    if let (Some(up), Some(out)) = (&uplink, uplink_live_tx) {
        // 中継サーバで聞いている人がいるチャンネルだけ Opus にして送る
        let (live_tx, live_rx) = mpsc::channel(256);
        audio_input.add_live_tap(live_tx, up.live_active_flag());
        tokio::spawn(up.clone().run_live_encoder(live_rx, out));
        log::info!("アップリンク: ライブ音声を送ります (Opus {} bps/ch)", config.uplink.live_bitrate);
    }

    // AudioOutputを作成して開始
    let output_device = if config.audio.output_device_id == "default" {
        None
    } else {
        Some(config.audio.output_device_id.as_str())
    };
    let mut audio_output = AudioOutput::new(output_device, config.audio.sample_rate)?;
    audio_output.set_health(tui_state.output_health.clone());
    audio_output.set_mute_flag(tui_state.output_muted.clone());
    let audio_output_tx = audio_output.start()?;
    audio_input.set_monitor(audio_output_tx, tui_state.clone());
    audio_input.start(channel_senders)?;

    log::info!("録音を開始しました (Ctrl+C または 'q' で停止)");

    // TUI に出すログイン URL(Remote サーバが動いているときだけ。中継サーバの URL はつながってから足す)
    let share_info = match (&remote_state, &config.remote) {
        (Some(state), Some(remote)) => Some(share::ShareInfo::new(
            remote,
            &config.tunnel,
            &state.login_path(),
        )),
        _ => None,
    };
    let mut tasks = Vec::new();
    if let Some(up) = uplink.clone() {
        // 中継サーバの状態と、閲覧画面のログイン URL(中継サーバから受け取る)を TUI に出す
        let tui_state_clone = tui_state.clone();
        let running_clone = running.clone();
        let base = uplink::public_base(&config.uplink);
        let local_share = share_info.clone();
        tasks.push(tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                *tui_state_clone.uplink_status.lock().unwrap_or_else(|e| e.into_inner()) = Some(up.status());
                if let (Some(base), Some(login)) = (&base, up.login_path()) {
                    let share = share::ShareInfo {
                        local: local_share.as_ref().and_then(|s| s.local.clone()),
                        public: Some(format!("{}{}", base, login)),
                    };
                    let mut cur = tui_state_clone.share_info.lock().unwrap_or_else(|e| e.into_inner());
                    if cur.as_ref() != Some(&share) {
                        *cur = Some(share);
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }));
    }

    *tui_state.share_info.lock().unwrap_or_else(|e| e.into_inner()) = share_info.clone();

    let tui_task = if show_tui {
        let tui_state_clone = tui_state.clone();
        let running_clone = running.clone();
        Some(tokio::spawn(async move {
            let mut tui_app = TuiApp::new(tui_state_clone, running_clone).with_share(share_info);
            if let Err(e) = tui_app.run().await {
                log::error!("TUIエラー: {e}");
            }
        }))
    } else {
        None
    };
    ready.store(true, Ordering::SeqCst);

    // 各チャンネルの処理タスクを起動

    // プロセッサをマップに格納（channel_id -> processor）
    let processors_map = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<
        usize,
        Arc<tokio::sync::Mutex<ChannelProcessor>>,
    >::new()));

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
            loop {
                // 停止後も受信済みチャンクを録音へ書き切ってから確定する。
                if !running_clone.load(Ordering::SeqCst) {
                    rx.close();
                }
                tokio::select! {
                    chunk = rx.recv() => {
                        let Some(chunk) = chunk else { break };
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
        let uplink_clone = uplink.clone();
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
                    if let Some(up) = &uplink_clone {
                        up.push_segment(&segment);
                    }
                }

                // 文字起こし結果をポーリング
                let results = proc.poll_transcripts().await;
                // 結果は受信時点で録音中(または直前)の区間にひも付ける
                let segment_id = proc.last_segment_id();
                if !results.is_empty() {
                    log::debug!(
                        "チャンネル {}: 文字起こし結果取得 {} 件",
                        channel_id,
                        results.len()
                    );
                    for mut result in results {
                        // GUI/TUI と保存・配信で同じ文字列を使う。
                        proc.add_transcript_to_tui(&result);

                        // 内容のある確定結果を記録する。
                        if !result.is_partial {
                            let cleaned_text = ChannelProcessor::normalize_transcript(&result.text);
                            if !cleaned_text.is_empty()
                                && !ChannelProcessor::is_punctuation_only(&cleaned_text)
                            {
                                // 前後の空白だけを整えて保存する。
                                result.text = cleaned_text;
                                if let Ok(json) = serde_json::to_string(&result) {
                                    log::info!("{}", json);
                                }

                                // Remote サーバ・中継サーバへ転送(同期、ブロックしない)
                                if let Some(state) = &remote_state_clone {
                                    state.push_with_segment(&result, segment_id);
                                }
                                if let Some(up) = &uplink_clone {
                                    up.push_transcript(&result, segment_id);
                                }
                            }
                        } else {
                            // partial も(設定で有効な場合のみ内部で振り分け)
                            if let Some(state) = &remote_state_clone {
                                state.push_with_segment(&result, segment_id);
                            }
                            if let Some(up) = &uplink_clone {
                                up.push_transcript(&result, segment_id);
                            }
                        }
                    }
                }
            }
        });
        tasks.push(transcript_task);
    }

    // タスク4: チャンネルの状態(受信中・音量・Transcribe 接続)を Remote サーバ・中継サーバへ。
    // 配信は変化があったときだけ(RemoteState::push_status / Uplink::push_status 側で間引く)
    if remote_state.is_some() || uplink.is_some() {
        let state = remote_state.clone();
        let uplink_clone = uplink.clone();
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
                if let Some(state) = &state {
                    state.push_status(&items);
                }
                if let Some(up) = &uplink_clone {
                    up.push_status(&items);
                }
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
    if let Some(t) = tunnel {
        t.stop().await;
    }

    // TUIタスクの完了を待つ
    if let Some(task) = tui_task {
        let _ = task.await;
    }

    // 他のタスクの完了を待つ
    for mut task in tasks {
        if tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
            .await
            .is_err()
        {
            log::warn!("停止待機が10秒を超えたタスクを中断します");
            task.abort();
            let _ = task.await;
        }
    }
    // チャンク処理との競合を避け、すべての書き込み終了後にWAV・区間録音を確定する。
    let map = processors_map.lock().await;
    for processor in map.values() {
        let mut processor = processor.lock().await;
        processor.stop().await?;
        // 停止で閉じた区間も中継サーバへ
        for segment in processor.take_finished_segments() {
            if let Some(up) = &uplink {
                up.push_segment(&segment);
            }
        }
    }
    drop(map);
    // 未送信分を送り切ってから(上限あり)アップリンクを閉じる
    let _ = uplink_stop_tx.send(true);
    if let Some(task) = uplink_task {
        let _ = task.await;
    }
    ready.store(false, Ordering::SeqCst);

    log::info!("dcr-transcribe を終了しました");

    Ok(())
}

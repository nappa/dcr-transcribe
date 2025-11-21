use crate::aws_transcribe::AwsTranscribeBackend;
use crate::buffer::AudioBuffer;
use crate::config::{BufferConfig, ChannelConfig, OutputConfig, TranscribeBackendType, TranscribeConfig, VadConfig, WhisperConfig};
use crate::transcribe::TranscribeClient;
use crate::transcribe_backend::TranscribeBackend;
use crate::tui_state::{TranscribeStatus, TuiState};
use crate::types::{AudioChunk, BufferedChunk, TranscriptResult, VadState};
use crate::vad::VoiceActivityDetector;
use crate::wav_writer::WavWriter;
use crate::whisper_api::WhisperBackend;
use anyhow::{Context, Result};
use tokio::sync::mpsc;

/// Transcribe API接続状態
#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscribeConnectionState {
    /// 未接続
    Disconnected,
    /// 接続中
    Connected,
}

/// 1つのチャンネルの完全な処理パイプライン
///
/// VAD、バッファリング、WAV書き出し、Transcribe送信を統合
pub struct ChannelProcessor {
    channel_id: usize,
    channel_name: String,
    vad: VoiceActivityDetector,
    vad_threshold_db: f32,
    buffer: AudioBuffer,
    wav_writer: WavWriter,
    transcribe_tx: Option<mpsc::Sender<Vec<i16>>>,
    transcribe_rx: Option<mpsc::Receiver<TranscriptResult>>,
    transcribe_backend: Option<Box<dyn TranscribeBackend>>,
    // 後方互換性のため残す（削除予定）
    #[allow(dead_code)]
    transcribe_client: Option<TranscribeClient>,
    sample_rate: u32,
    tui_state: Option<TuiState>,
    /// 音声出力用Sender (オプション)
    audio_output_tx: Option<mpsc::Sender<Vec<i16>>>,
    /// Transcribe接続状態
    connection_state: TranscribeConnectionState,
    /// 無音継続時間（ミリ秒）
    silence_duration_ms: u32,
    /// 接続切断の無音閾値（ミリ秒）
    silence_threshold_ms: u32,
    /// 切断中に蓄積された音声サンプル
    buffered_samples_during_disconnect: Vec<Vec<i16>>,
    /// 起動時に接続するか
    connect_on_startup: bool,
    /// 再接続時にバッファを送信するか
    send_buffered_on_reconnect: bool,
}

impl ChannelProcessor {
    pub async fn new(
        channel_config: &ChannelConfig,
        vad_config: &VadConfig,
        buffer_config: &BufferConfig,
        transcribe_config: &TranscribeConfig,
        whisper_config: Option<&WhisperConfig>,
        output_config: &OutputConfig,
        sample_rate: u32,
    ) -> Result<Self> {
        let vad = VoiceActivityDetector::new(vad_config, sample_rate);
        let buffer = AudioBuffer::new(buffer_config, sample_rate);
        let wav_writer = WavWriter::new(
            channel_config.id,
            &output_config.wav_output_dir,
            sample_rate,
        )?;

        // バックエンドを選択して作成
        let transcribe_backend: Box<dyn TranscribeBackend> = match transcribe_config.backend {
            TranscribeBackendType::Aws => {
                log::info!("チャンネル {}: Amazon Transcribe バックエンドを使用", channel_config.id);
                Box::new(
                    AwsTranscribeBackend::new(transcribe_config.clone(), channel_config.id)
                        .await
                        .context("Amazon Transcribe バックエンド作成失敗")?,
                )
            }
            TranscribeBackendType::Whisper => {
                log::info!("チャンネル {}: OpenAI Whisper API バックエンドを使用", channel_config.id);
                let whisper_cfg = whisper_config
                    .ok_or_else(|| anyhow::anyhow!("Whisper設定が見つかりません"))?;

                // WhisperConfig を作成
                let whisper_backend_config = crate::whisper_api::WhisperConfig {
                    api_key: whisper_cfg.api_key.clone(),
                    model: whisper_cfg.model.clone(),
                    language: whisper_cfg.language.clone(),
                    sample_rate: whisper_cfg.sample_rate,
                    chunk_duration_secs: whisper_cfg.chunk_duration_secs,
                };

                Box::new(
                    WhisperBackend::new(whisper_backend_config, channel_config.id)
                        .await
                        .context("Whisper API バックエンド作成失敗")?,
                )
            }
        };

        Ok(Self {
            channel_id: channel_config.id,
            channel_name: channel_config.name.clone(),
            vad,
            vad_threshold_db: vad_config.threshold_db,
            buffer,
            wav_writer,
            transcribe_tx: None,
            transcribe_rx: None,
            transcribe_backend: Some(transcribe_backend),
            transcribe_client: None,
            sample_rate,
            tui_state: None,
            audio_output_tx: None,
            connection_state: TranscribeConnectionState::Disconnected,
            silence_duration_ms: 0,
            silence_threshold_ms: vad_config.silence_disconnect_threshold_ms,
            buffered_samples_during_disconnect: Vec::new(),
            connect_on_startup: transcribe_config.connect_on_startup,
            send_buffered_on_reconnect: transcribe_config.send_buffered_on_reconnect,
        })
    }

    /// TUI状態を設定
    pub fn set_tui_state(&mut self, tui_state: TuiState) {
        // VAD閾値をTUI状態に設定
        tui_state.update_channel(self.channel_id, |channel| {
            channel.set_vad_threshold(self.vad_threshold_db);
        });
        self.tui_state = Some(tui_state);
    }

    /// 音声出力用Senderを設定
    pub fn set_audio_output(&mut self, tx: mpsc::Sender<Vec<i16>>) {
        self.audio_output_tx = Some(tx);
    }

    /// 音声出力用Senderをクリア
    pub fn clear_audio_output(&mut self) {
        self.audio_output_tx = None;
    }

    /// 処理を開始
    pub async fn start(&mut self) -> Result<()> {
        log::info!(
            "チャンネル {} ({}) の処理を開始",
            self.channel_id,
            self.channel_name
        );

        // WAVファイル書き込みを開始
        self.wav_writer.start()?;

        // connect_on_startupがtrueの場合のみ起動時に接続
        if self.connect_on_startup {
            log::info!(
                "チャンネル {}: 起動時にTranscribe接続を開始",
                self.channel_id
            );
            self.reconnect_transcribe().await?;
        } else {
            log::info!(
                "チャンネル {}: 音声検出まで接続を待機",
                self.channel_id
            );
            // TUI状態を未接続に設定
            if let Some(tui_state) = &self.tui_state {
                tui_state.update_channel(self.channel_id, |channel| {
                    channel.update_transcribe_status(TranscribeStatus::Disconnected);
                });
            }
        }

        Ok(())
    }

    /// 音声チャンクを処理
    pub async fn process_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        let samples = &chunk.samples;

        // 1. WAVファイルに書き込み（無音含む全データ）
        self.wav_writer.write_samples(samples)?;

        // 2. バッファに追加
        self.buffer.push(BufferedChunk {
            samples: samples.clone(),
            timestamp_ns: chunk.timestamp_ns,
        });

        // 3. VADで音声区間を判定
        let is_voice = self.vad.process(samples);
        let volume_db = self.vad.get_last_volume_db();

        // 4. TUI状態を更新
        if let Some(tui_state) = &self.tui_state {
            let volume_db = self.vad.get_last_volume_db();
            let vad_state = self.vad.get_state();
            tui_state.update_channel(self.channel_id, |channel| {
                channel.update_volume(volume_db);
                channel.update_vad_state(vad_state);
            });
        }

        // 5. チャンク時間を計算（ミリ秒）
        let chunk_duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u32;

        // 6. 接続状態に応じた処理
        match (is_voice, &self.connection_state) {
            // 音声検出 + 未接続 → 再接続 + バッファ送信
            (true, TranscribeConnectionState::Disconnected) => {
                // バッファサイズを計算（メトリクス収集）
                let total_buffered_samples: usize = self.buffered_samples_during_disconnect
                    .iter()
                    .map(|chunk| chunk.len())
                    .sum();
                let buffered_duration_ms = (total_buffered_samples as f64 / self.sample_rate as f64 * 1000.0) as u32;

                log::info!(
                    "チャンネル {}: ★音声検出★ Transcribe再接続を開始 (音量: {:.2} dB, バッファ: {}チャンク, {}ms相当)",
                    self.channel_id,
                    volume_db,
                    self.buffered_samples_during_disconnect.len(),
                    buffered_duration_ms
                );
                self.reconnect_transcribe().await?;

                // 再接続時にバッファ送信が有効な場合
                if self.send_buffered_on_reconnect && !self.buffered_samples_during_disconnect.is_empty() {
                    log::info!(
                        "チャンネル {}: 切断中の音声バッファを送信（{}チャンク, {}ms相当）",
                        self.channel_id,
                        self.buffered_samples_during_disconnect.len(),
                        buffered_duration_ms
                    );

                    // バッファを送信
                    if let Some(tx) = &self.transcribe_tx {
                        for buffered in &self.buffered_samples_during_disconnect {
                            if let Err(e) = tx.send(buffered.clone()).await {
                                log::error!(
                                    "チャンネル {}: バッファ送信に失敗: {}",
                                    self.channel_id,
                                    e
                                );
                                break;
                            }
                        }
                    }
                }

                // バッファをクリア
                self.buffered_samples_during_disconnect.clear();

                // 現在のチャンクを送信
                if let Some(tx) = &self.transcribe_tx {
                    if let Err(e) = tx.send(samples.clone()).await {
                        log::error!(
                            "チャンネル {}: Transcribeへの送信に失敗: {} - 切断して次回再接続します",
                            self.channel_id,
                            e
                        );
                        // チャンネルが閉じられた場合は切断状態に移行
                        self.transcribe_tx = None;
                        self.connection_state = TranscribeConnectionState::Disconnected;

                        if let Some(tui_state) = &self.tui_state {
                            tui_state.update_channel(self.channel_id, |channel| {
                                channel.update_transcribe_status(TranscribeStatus::Disconnected);
                            });
                        }
                    }
                }

                self.silence_duration_ms = 0;
            }

            // 音声検出 + 接続中 → 通常送信
            (true, TranscribeConnectionState::Connected) => {
                self.silence_duration_ms = 0;

                if let Some(tx) = &self.transcribe_tx {
                    if let Err(e) = tx.send(samples.clone()).await {
                        log::error!(
                            "チャンネル {}: Transcribeへの送信に失敗: {} - 切断して次回再接続します",
                            self.channel_id,
                            e
                        );
                        // チャンネルが閉じられた場合は切断状態に移行
                        self.transcribe_tx = None;
                        self.connection_state = TranscribeConnectionState::Disconnected;

                        // エラー時はTUI状態を切断に更新
                        if let Some(tui_state) = &self.tui_state {
                            tui_state.update_channel(self.channel_id, |channel| {
                                channel.update_transcribe_status(TranscribeStatus::Disconnected);
                            });
                        }
                    } else if let Some(tui_state) = &self.tui_state {
                        // 正常送信時
                        tui_state.update_channel(self.channel_id, |channel| {
                            channel.update_transcribe_status(TranscribeStatus::Connected);
                        });
                    }
                }
            }

            // 無音 + 接続中 → カウント増加、閾値超過で切断
            (false, TranscribeConnectionState::Connected) => {
                self.silence_duration_ms += chunk_duration_ms;

                if self.silence_duration_ms >= self.silence_threshold_ms {
                    log::info!(
                        "チャンネル {}: 無音が{}ms継続、Transcribe接続を停止 (閾値: {}ms)",
                        self.channel_id,
                        self.silence_duration_ms,
                        self.silence_threshold_ms
                    );
                    self.disconnect_transcribe().await?;
                } else {
                    // 閾値未満の場合はゼロサンプル送信（既存の挙動）
                    if let Some(tx) = &self.transcribe_tx {
                        let zero_samples = vec![0i16; samples.len()];
                        if let Err(e) = tx.send(zero_samples).await {
                            log::error!(
                                "チャンネル {}: ゼロサンプル送信に失敗: {} - 切断して次回再接続します",
                                self.channel_id,
                                e
                            );
                            // チャンネルが閉じられた場合は切断状態に移行
                            self.transcribe_tx = None;
                            self.connection_state = TranscribeConnectionState::Disconnected;

                            if let Some(tui_state) = &self.tui_state {
                                tui_state.update_channel(self.channel_id, |channel| {
                                    channel.update_transcribe_status(TranscribeStatus::Disconnected);
                                });
                            }
                        }
                    }
                }
            }

            // 無音 + 未接続 → 何もしない（バッファに蓄積しない）
            (false, TranscribeConnectionState::Disconnected) => {
                // 切断中の無音はバッファに蓄積しない
                // これにより、再接続時の遅延を防ぐ
            }
        }

        // 7. 音声出力デバイスに送信（設定されている場合）
        if let Some(tx) = &self.audio_output_tx {
            if let Err(e) = tx.send(samples.clone()).await {
                log::warn!(
                    "チャンネル {}: 音声出力への送信に失敗: {}",
                    self.channel_id,
                    e
                );
            }
        }

        Ok(())
    }

    /// Transcribe APIに再接続
    async fn reconnect_transcribe(&mut self) -> Result<()> {
        // 既に接続中の場合は何もしない
        if self.connection_state == TranscribeConnectionState::Connected {
            return Ok(());
        }

        log::info!("チャンネル {}: Transcribe再接続開始", self.channel_id);

        // バックエンドから新しいストリームを開始
        if let Some(mut backend) = self.transcribe_backend.take() {
            // start_timeをリセット（タイムスタンプドリフト防止）
            backend.reset_start_time();
            match backend.start_stream().await {
                Ok((tx, rx)) => {
                    self.transcribe_tx = Some(tx);
                    self.transcribe_rx = Some(rx);
                    self.transcribe_backend = Some(backend);
                    self.connection_state = TranscribeConnectionState::Connected;

                    // TUI状態を接続中に更新
                    if let Some(tui_state) = &self.tui_state {
                        tui_state.update_channel(self.channel_id, |channel| {
                            channel.update_transcribe_status(TranscribeStatus::Connected);
                        });
                    }

                    log::info!(
                        "チャンネル {}: Transcribe再接続成功 (無音閾値: {}ms)",
                        self.channel_id,
                        self.silence_threshold_ms
                    );
                    Ok(())
                }
                Err(e) => {
                    // エラー時もバックエンドを戻す
                    self.transcribe_backend = Some(backend);

                    // TUI状態をエラーに更新
                    if let Some(tui_state) = &self.tui_state {
                        tui_state.update_channel(self.channel_id, |channel| {
                            channel.update_transcribe_status(TranscribeStatus::Error);
                        });
                    }

                    log::error!("チャンネル {}: Transcribe再接続失敗: {}", self.channel_id, e);
                    Err(e)
                }
            }
        } else {
            Ok(())
        }
    }

    /// Transcribe API接続を切断
    async fn disconnect_transcribe(&mut self) -> Result<()> {
        log::info!("チャンネル {}: Transcribe接続を停止", self.channel_id);

        // 送信チャンネルをドロップすることで接続終了
        self.transcribe_tx = None;
        self.connection_state = TranscribeConnectionState::Disconnected;
        self.silence_duration_ms = 0;

        // TUI状態を未接続に更新
        if let Some(tui_state) = &self.tui_state {
            tui_state.update_channel(self.channel_id, |channel| {
                channel.update_transcribe_status(TranscribeStatus::Disconnected);
            });
        }

        Ok(())
    }

    /// 文字起こし結果を取得（non-blocking）
    pub async fn poll_transcripts(&mut self) -> Vec<TranscriptResult> {
        let mut results = Vec::new();

        if let Some(rx) = &mut self.transcribe_rx {
            // 利用可能な全ての結果を取得
            while let Ok(result) = rx.try_recv() {
                log::debug!(
                    "チャンネル {}: 文字起こし結果受信 - テキスト: '{}', 部分結果: {}",
                    self.channel_id,
                    result.text,
                    result.is_partial
                );
                results.push(result);
            }
        } else {
            // transcribe_rxがNoneの場合（未接続または切断中）
            if self.connection_state == TranscribeConnectionState::Disconnected {
                log::trace!("チャンネル {}: Transcribe未接続のため結果なし", self.channel_id);
            }
        }

        results
    }

    /// 処理を停止
    pub async fn stop(&mut self) -> Result<()> {
        log::info!(
            "チャンネル {} ({}) の処理を停止",
            self.channel_id,
            self.channel_name
        );

        // Transcribeストリームをクローズ
        self.transcribe_tx = None;

        // WAVファイルを終了
        self.wav_writer.finalize()?;

        Ok(())
    }

    /// チャンネルIDを取得
    pub fn channel_id(&self) -> usize {
        self.channel_id
    }

    /// チャンネル名を取得
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// WAV書き込み時間を取得
    pub fn wav_duration_seconds(&self) -> f64 {
        self.wav_writer.duration_seconds()
    }

    /// バッファサイズを取得
    pub fn buffer_duration_seconds(&self) -> f64 {
        self.buffer.duration_seconds()
    }

    /// VAD状態を取得
    pub fn vad_state(&self) -> VadState {
        self.vad.get_state()
    }

    /// 最新のボリューム（dB）を取得
    pub fn current_volume_db(&self) -> f32 {
        self.vad.get_last_volume_db()
    }

    /// フィラーワード（言い淀み）を削除
    pub fn remove_filler_words(text: &str) -> String {
        // 削除対象のフィラーワードリスト
        let filler_words = [
            "えっと",
            "あの",
            "ええと",
            "ええ",
            "えー",
            "えーと",
            "あのー",
            "っと",
            "っとー",
        ];

        let mut result = text.to_string();

        // 各フィラーワードを削除
        for filler in &filler_words {
            // 完全一致する単語を削除（前後に空白がある場合）
            result = result.replace(&format!("{} ", filler), "");
            result = result.replace(&format!(" {}", filler), "");
            // 文頭・文末の場合
            if result.starts_with(filler) {
                result = result[filler.len()..].to_string();
            }
            if result.ends_with(filler) {
                result = result[..result.len() - filler.len()].to_string();
            }
        }

        // 連続する空白を1つにまとめる
        while result.contains("  ") {
            result = result.replace("  ", " ");
        }

        // 前後の空白を削除
        result.trim().to_string()
    }

    /// 句読点のみの行かどうかをチェック
    pub fn is_punctuation_only(text: &str) -> bool {
        let trimmed = text.trim();

        // 空文字列の場合はtrue
        if trimmed.is_empty() {
            return true;
        }

        // 句読点のみで構成されているかチェック
        // 「、」「。」「と。」のような組み合わせ
        let allowed_chars = ['、', '。', 'と'];

        // すべての文字が許可された文字かチェック
        let all_punctuation = trimmed.chars().all(|c| allowed_chars.contains(&c));

        all_punctuation
    }

    /// TUI状態にTranscribe結果を追加
    pub fn add_transcript_to_tui(&self, result: &TranscriptResult) {
        if let Some(tui_state) = &self.tui_state {
            let text_to_display = if result.is_partial {
                // 部分結果はフィラーワード削除しない（リアルタイム性を優先）
                result.text.clone()
            } else {
                // 確定結果のみフィラーワードを削除
                let cleaned_text = Self::remove_filler_words(&result.text);

                // 空文字列または句読点のみの場合は追加しない
                if cleaned_text.is_empty() || Self::is_punctuation_only(&cleaned_text) {
                    return;
                }

                cleaned_text
            };

            tui_state.update_channel(self.channel_id, |channel| {
                channel.add_transcript(
                    text_to_display,
                    result.timestamp.clone(),
                    result.timestamp_seconds,
                    result.is_partial,
                    result.stability,
                );
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AudioFormat;

    #[tokio::test]
    #[ignore] // AWS認証情報が必要なため、通常はスキップ
    async fn test_channel_processor_creation() {
        let channel_config = ChannelConfig {
            id: 0,
            name: "テストチャンネル".to_string(),
            enabled: true,
        };

        let vad_config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
            silence_disconnect_threshold_ms: 10000,
        };

        let buffer_config = BufferConfig {
            capacity_seconds: 30,
            drop_policy: crate::types::DropPolicy::DropOldest,
        };

        let transcribe_config = TranscribeConfig {
            backend: TranscribeBackendType::Aws,
            region: "ap-northeast-1".to_string(),
            language_code: "ja-JP".to_string(),
            sample_rate: 16000,
            max_retries: 3,
            timeout_seconds: 10,
            connect_on_startup: false,
            send_buffered_on_reconnect: true,
        };

        let output_config = OutputConfig {
            wav_output_dir: "/tmp/test_recordings".to_string(),
            log_level: "info".to_string(),
        };

        let result = ChannelProcessor::new(
            &channel_config,
            &vad_config,
            &buffer_config,
            &transcribe_config,
            None, // whisper_config
            &output_config,
            16000,
        )
        .await;

        assert!(result.is_ok());
    }
}

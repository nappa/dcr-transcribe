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

        // Transcribeストリームを開始
        if let Some(mut backend) = self.transcribe_backend.take() {
            let (tx, rx) = backend.start_stream().await?;
            self.transcribe_tx = Some(tx);
            self.transcribe_rx = Some(rx);
            self.transcribe_backend = Some(backend);
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

        // 4. TUI状態を更新
        if let Some(tui_state) = &self.tui_state {
            let volume_db = self.vad.get_last_volume_db();
            let vad_state = self.vad.get_state();
            tui_state.update_channel(self.channel_id, |channel| {
                channel.update_volume(volume_db);
                channel.update_vad_state(vad_state);
            });
        }

        // 5. PCMサンプルをTranscribeに送信
        if let Some(tx) = &self.transcribe_tx {
            let samples_to_send = if is_voice {
                samples.clone()
            } else {
                // 無音部分はゼロサンプルに置換
                vec![0i16; samples.len()]
            };

            if let Err(e) = tx.send(samples_to_send).await {
                log::error!(
                    "チャンネル {}: Transcribeへの送信に失敗: {}",
                    self.channel_id,
                    e
                );
                // エラー時はTUI状態を更新
                if let Some(tui_state) = &self.tui_state {
                    tui_state.update_channel(self.channel_id, |channel| {
                        channel.update_transcribe_status(TranscribeStatus::Error);
                    });
                }
            } else if let Some(tui_state) = &self.tui_state {
                // 正常送信時
                tui_state.update_channel(self.channel_id, |channel| {
                    channel.update_transcribe_status(TranscribeStatus::Connected);
                });
            }
        }

        // 6. 音声出力デバイスに送信（設定されている場合）
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

    /// 文字起こし結果を取得（non-blocking）
    pub async fn poll_transcripts(&mut self) -> Vec<TranscriptResult> {
        let mut results = Vec::new();

        if let Some(rx) = &mut self.transcribe_rx {
            // 利用可能な全ての結果を取得
            while let Ok(result) = rx.try_recv() {
                results.push(result);
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
        };

        let buffer_config = BufferConfig {
            capacity_seconds: 30,
            drop_policy: crate::types::DropPolicy::DropOldest,
        };

        let transcribe_config = TranscribeConfig {
            region: "ap-northeast-1".to_string(),
            language_code: "ja-JP".to_string(),
            sample_rate: 16000,
            max_retries: 3,
            timeout_seconds: 10,
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
            &output_config,
            16000,
        )
        .await;

        assert!(result.is_ok());
    }
}

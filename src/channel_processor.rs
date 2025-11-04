use crate::buffer::AudioBuffer;
use crate::config::{BufferConfig, ChannelConfig, OutputConfig, TranscribeConfig, VadConfig};
use crate::transcribe::TranscribeClient;
use crate::types::{AudioChunk, BufferedChunk, TranscriptResult};
use crate::vad::VoiceActivityDetector;
use crate::wav_writer::WavWriter;
use anyhow::Result;
use tokio::sync::mpsc;

/// 1つのチャンネルの完全な処理パイプライン
///
/// VAD、バッファリング、WAV書き出し、Transcribe送信を統合
pub struct ChannelProcessor {
    channel_id: usize,
    channel_name: String,
    vad: VoiceActivityDetector,
    buffer: AudioBuffer,
    wav_writer: WavWriter,
    transcribe_tx: Option<mpsc::Sender<Vec<i16>>>,
    transcribe_rx: Option<mpsc::Receiver<TranscriptResult>>,
    transcribe_client: Option<TranscribeClient>,
    sample_rate: u32,
}

impl ChannelProcessor {
    pub async fn new(
        channel_config: &ChannelConfig,
        vad_config: &VadConfig,
        buffer_config: &BufferConfig,
        transcribe_config: &TranscribeConfig,
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

        // Transcribeクライアントを作成
        let transcribe_client =
            TranscribeClient::new(transcribe_config.clone(), channel_config.id).await?;

        Ok(Self {
            channel_id: channel_config.id,
            channel_name: channel_config.name.clone(),
            vad,
            buffer,
            wav_writer,
            transcribe_tx: None,
            transcribe_rx: None,
            transcribe_client: Some(transcribe_client),
            sample_rate,
        })
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
        if let Some(mut client) = self.transcribe_client.take() {
            let (tx, rx) = client.start_stream().await?;
            self.transcribe_tx = Some(tx);
            self.transcribe_rx = Some(rx);
            self.transcribe_client = Some(client);
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

        // 4. PCMサンプルをTranscribeに送信
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

use crate::transcribe_backend::TranscribeBackend;
use crate::types::TranscriptResult;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;
use std::io::Cursor;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// OpenAI Whisper API設定
#[derive(Debug, Clone)]
pub struct WhisperConfig {
    pub api_key: String,
    pub model: String,         // "whisper-1"
    pub language: Option<String>, // "ja", "en", など
    pub sample_rate: u32,
    pub chunk_duration_secs: u64, // 音声チャンクをためる時間（秒）
}

/// OpenAI Whisper API レスポンス
#[derive(Debug, Deserialize)]
struct WhisperResponse {
    text: String,
}

/// OpenAI Whisper API バックエンド
pub struct WhisperBackend {
    config: WhisperConfig,
    channel_id: usize,
    start_time: SystemTime,
    client: reqwest::Client,
    /// 再接続回数（メトリクス収集用）
    reconnection_count: u32,
    /// 現在実行中のタスクハンドル（リソースリーク防止用）
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl WhisperBackend {
    pub async fn new(config: WhisperConfig, channel_id: usize, start_time: SystemTime) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Whisper API HTTPクライアント作成失敗")?;

        Ok(Self {
            config,
            channel_id,
            start_time,
            client,
            reconnection_count: 0,
            task_handle: None,
        })
    }

    /// PCMデータをWAVフォーマットに変換
    fn pcm_to_wav(&self, pcm_data: &[i16]) -> Result<Vec<u8>> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.config.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec)
                .context("WAVライター作成失敗")?;

            for &sample in pcm_data {
                writer.write_sample(sample).context("WAV書き込み失敗")?;
            }

            writer.finalize().context("WAV finalize失敗")?;
        }

        Ok(cursor.into_inner())
    }

    /// Whisper APIを呼び出して文字起こし
    async fn transcribe_audio(&self, wav_data: Vec<u8>) -> Result<String> {
        let part = multipart::Part::bytes(wav_data)
            .file_name("audio.wav")
            .mime_str("audio/wav")?;

        let mut form = multipart::Form::new()
            .part("file", part)
            .text("model", self.config.model.clone());

        if let Some(ref language) = self.config.language {
            form = form.text("language", language.clone());
        }

        let response = self
            .client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .multipart(form)
            .send()
            .await
            .context("Whisper API リクエスト失敗")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Whisper API エラー: {} - {}", status, error_text);
        }

        let whisper_response: WhisperResponse = response
            .json::<WhisperResponse>()
            .await
            .context("Whisper API レスポンスパース失敗")?;

        Ok(whisper_response.text)
    }
}

#[async_trait]
impl TranscribeBackend for WhisperBackend {
    async fn start_stream(
        &mut self,
    ) -> Result<(mpsc::Sender<Vec<i16>>, mpsc::Receiver<TranscriptResult>)> {
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<i16>>(4096);
        let audio_rx = Arc::new(Mutex::new(audio_rx));
        let (result_tx, result_rx) = mpsc::channel::<TranscriptResult>(32);

        let sample_rate = self.config.sample_rate;
        let chunk_duration_secs = self.config.chunk_duration_secs;
        let channel_id = self.channel_id;
        let start_time = self.start_time;
        let config = self.config.clone();
        let client = self.client.clone();

        // 古いタスクがあれば破棄（チャンネルクローズにより自動終了）
        if let Some(old_handle) = self.task_handle.take() {
            log::debug!("チャンネル {}: 古いWhisperタスクを破棄", channel_id);
            // タスクハンドルをドロップすることで、バックグラウンドで終了させる
            drop(old_handle);
        }

        let handle = tokio::spawn(async move {
            use tokio::time::{Duration, timeout};

            let mut pcm_buffer: Vec<i16> = Vec::new();
            let samples_per_chunk = (sample_rate as u64 * chunk_duration_secs) as usize;

            loop {
                let mut rx = audio_rx.lock().await;

                // データを待機（最大2秒）
                match timeout(Duration::from_secs(2), rx.recv()).await {
                    Ok(Some(samples)) => {
                        drop(rx); // ロックを解放

                        pcm_buffer.extend_from_slice(&samples);

                        // バッファが一定サイズに達したら文字起こし
                        if pcm_buffer.len() >= samples_per_chunk {
                            let to_transcribe: Vec<i16> = pcm_buffer.drain(..).collect();

                            log::debug!("Whisper API: {} サンプルを文字起こし中", to_transcribe.len());

                            // WAVに変換
                            let backend = WhisperBackend {
                                config: config.clone(),
                                channel_id,
                                start_time,
                                client: client.clone(),
                                reconnection_count: 0,
                                task_handle: None,
                            };

                            match backend.pcm_to_wav(&to_transcribe) {
                                Ok(wav_data) => {
                                    log::debug!("Whisper API: WAVデータサイズ {} バイト", wav_data.len());

                                    // Whisper APIを呼び出し
                                    match backend.transcribe_audio(wav_data).await {
                                        Ok(text) => {
                                            if !text.is_empty() {
                                                log::debug!("Whisper API: 文字起こし結果 - {}", text);
                                                let transcript = TranscriptResult::new(
                                                    channel_id,
                                                    text,
                                                    false, // Whisper APIは常に最終結果
                                                    None,  // Whisperはstabilityなし
                                                    start_time,
                                                );
                                                if let Err(e) = result_tx.try_send(transcript) {
                                                    log::warn!("Whisper API 結果送信失敗: {}", e);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            log::error!("Whisper API 文字起こし失敗: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!("WAV変換失敗: {}", e);
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        log::debug!("WhisperBackend: チャンネルクローズ");

                        // 残りのバッファを処理
                        if !pcm_buffer.is_empty() {
                            log::debug!("Whisper API: 残りの {} サンプルを文字起こし中", pcm_buffer.len());

                            let backend = WhisperBackend {
                                config: config.clone(),
                                channel_id,
                                start_time,
                                client: client.clone(),
                                reconnection_count: 0,
                                task_handle: None,
                            };

                            match backend.pcm_to_wav(&pcm_buffer) {
                                Ok(wav_data) => {
                                    match backend.transcribe_audio(wav_data).await {
                                        Ok(text) => {
                                            if !text.is_empty() {
                                                let transcript = TranscriptResult::new(
                                                    channel_id,
                                                    text,
                                                    false,
                                                    None,
                                                    start_time,
                                                );
                                                let _ = result_tx.try_send(transcript);
                                            }
                                        }
                                        Err(e) => {
                                            log::error!("Whisper API 最終文字起こし失敗: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!("WAV変換失敗: {}", e);
                                }
                            }
                        }
                        break;
                    }
                    Err(_) => {
                        // タイムアウト - ループを続ける
                        drop(rx); // ロックを解放
                    }
                }
            }
        });

        // タスクハンドルを保存（リソースリーク防止）
        self.task_handle = Some(handle);

        Ok((audio_tx, result_rx))
    }

    fn channel_id(&self) -> usize {
        self.channel_id
    }
}

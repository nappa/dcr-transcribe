// reconnection_count は計測/将来用で未使用
#![allow(dead_code)]

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
        self.transcribe_audio_with_url(
            "https://api.openai.com/v1/audio/transcriptions",
            wav_data,
        )
        .await
    }

    /// 指定 URL に対して文字起こしリクエストを送る(テスト時にモックサーバへ向ける用)。
    ///
    /// リトライ仕様:
    /// - 429 (rate limit) / 5xx (server error) / network error は最大 3 回まで再送
    /// - 4xx (auth/format error) は即座に諦める
    /// - 指数バックオフ(500ms → 1s → 2s)
    async fn transcribe_audio_with_url(&self, url: &str, wav_data: Vec<u8>) -> Result<String> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut backoff_ms: u64 = 500;

        for attempt in 1..=MAX_ATTEMPTS {
            // multipart::Form は consume されるので毎回作り直す
            let part = multipart::Part::bytes(wav_data.clone())
                .file_name("audio.wav")
                .mime_str("audio/wav")?;
            let mut form = multipart::Form::new()
                .part("file", part)
                .text("model", self.config.model.clone());
            if let Some(ref language) = self.config.language {
                form = form.text("language", language.clone());
            }

            let send_result = self
                .client
                .post(url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .multipart(form)
                .send()
                .await;

            // ネットワークエラー
            let response = match send_result {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(anyhow::anyhow!("Whisper API リクエスト失敗: {}", e));
                    }
                    log::warn!(
                        "Whisper API: ネットワークエラー(試行 {}/{}, {}ms 後リトライ): {}",
                        attempt, MAX_ATTEMPTS, backoff_ms, e
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = backoff_ms.saturating_mul(2);
                    continue;
                }
            };

            let status = response.status();
            if status.is_success() {
                return response
                    .json::<WhisperResponse>()
                    .await
                    .context("Whisper API レスポンスパース失敗")
                    .map(|r| r.text);
            }

            // リトライ可能か判定
            let retryable = status.as_u16() == 429 || status.is_server_error();
            let body = response.text().await.unwrap_or_default();

            if !retryable || attempt >= MAX_ATTEMPTS {
                anyhow::bail!("Whisper API エラー: {} - {}", status, body);
            }

            log::warn!(
                "Whisper API: HTTP {}(試行 {}/{}, {}ms 後リトライ): {}",
                status, attempt, MAX_ATTEMPTS, backoff_ms, body
            );
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = backoff_ms.saturating_mul(2);
        }

        unreachable!("retry loop should always exit via Ok or Err");
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

        // 古いタスクがあれば明示的に abort する。
        // 注: tokio の JoinHandle は drop しても task は止まらない(detach されるだけ)。
        // 前バージョンは drop に頼っていたが、確実にクリーンアップするため abort() に変更。
        if let Some(old_handle) = self.task_handle.take() {
            log::debug!("チャンネル {}: 古い Whisper タスクを abort", channel_id);
            old_handle.abort();
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

impl Drop for WhisperBackend {
    /// バックエンドが drop されるときに、生きているバックグラウンドタスクを確実に停止する。
    fn drop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn cfg() -> WhisperConfig {
        WhisperConfig {
            api_key: "test-key".to_string(),
            model: "whisper-1".to_string(),
            language: Some("ja".to_string()),
            sample_rate: 16000,
            chunk_duration_secs: 5,
        }
    }

    async fn backend() -> WhisperBackend {
        WhisperBackend::new(cfg(), 0, SystemTime::now())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn pcm_to_wav_produces_valid_riff_header() {
        let b = backend().await;
        let pcm: Vec<i16> = (0..1600).map(|i| i as i16).collect();
        let wav = b.pcm_to_wav(&pcm).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert!(wav.len() > 44, "WAV header + data should be > 44 bytes");
    }

    #[tokio::test]
    async fn pcm_to_wav_handles_empty_input() {
        let b = backend().await;
        let wav = b.pcm_to_wav(&[]).unwrap();
        // ヘッダのみ
        assert_eq!(&wav[0..4], b"RIFF");
    }

    #[tokio::test]
    async fn transcribe_succeeds_on_200() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"こんにちは"}"#)
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await.unwrap();

        assert_eq!(r, "こんにちは");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_retries_on_429_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let mock_429 = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(429)
            .with_body("rate limited")
            .expect(1)
            .create_async()
            .await;
        let mock_ok = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"recovered"}"#)
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await.unwrap();

        assert_eq!(r, "recovered");
        mock_429.assert_async().await;
        mock_ok.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_retries_on_500_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let mock_500 = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(500)
            .with_body("internal error")
            .expect(1)
            .create_async()
            .await;
        let mock_ok = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"after-5xx"}"#)
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await.unwrap();

        assert_eq!(r, "after-5xx");
        mock_500.assert_async().await;
        mock_ok.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_does_not_retry_on_401() {
        // 認証エラーは即座に諦める(リトライしない)
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(401)
            .with_body("unauthorized")
            .expect(1) // 1 回しか呼ばれないこと
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await;

        assert!(r.is_err(), "401 should produce Err");
        let err_msg = format!("{}", r.unwrap_err());
        assert!(err_msg.contains("401"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_does_not_retry_on_400() {
        // 4xx (Bad Request) もリトライしない
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(400)
            .with_body("bad request")
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await;

        assert!(r.is_err());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_gives_up_after_max_retries() {
        // 永続的に 500 を返すと、3 回試行して諦める
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(503)
            .with_body("service unavailable")
            .expect(3) // 3 回呼ばれる
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await;

        assert!(r.is_err(), "should fail after max retries");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_passes_authorization_header() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"ok"}"#)
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await.unwrap();
        assert_eq!(r, "ok");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_includes_language_in_form() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .match_body(mockito::Matcher::Regex(r#"name="language""#.to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"ok"}"#)
            .expect(1)
            .create_async()
            .await;

        let b = backend().await; // language = Some("ja")
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let _ = b.transcribe_audio_with_url(&url, wav).await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transcribe_handles_invalid_json_response() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not valid json")
            .expect(1)
            .create_async()
            .await;

        let b = backend().await;
        let wav = b.pcm_to_wav(&vec![0i16; 16]).unwrap();
        let url = format!("{}/v1/audio/transcriptions", server.url());
        let r = b.transcribe_audio_with_url(&url, wav).await;
        assert!(r.is_err(), "invalid JSON should produce Err");
    }
}

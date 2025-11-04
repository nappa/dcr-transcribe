use crate::config::TranscribeConfig;
use crate::types::TranscriptResult;
use anyhow::Result;
use aws_config;
use aws_sdk_transcribestreaming::Client as AwsTranscribeClient;
use aws_sdk_transcribestreaming::types::{AudioEvent, AudioStream, LanguageCode, MediaEncoding};
use aws_smithy_types::Blob;
use std::time::SystemTime;
use tokio::sync::mpsc;
// use std::io::Cursor;
use async_stream::stream;
// use claxon;

/// AWS Transcribe Streaming API クライアント
///
/// リトライ機構とバックオフを実装
pub struct TranscribeClient {
    config: TranscribeConfig,
    channel_id: usize,
    start_time: SystemTime,
    retry_count: u32,
}

impl TranscribeClient {
    pub async fn new(config: TranscribeConfig, channel_id: usize) -> Result<Self> {
        Ok(Self {
            config,
            channel_id,
            start_time: SystemTime::now(),
            retry_count: 0,
        })
    }

    /// ストリーミング文字起こしセッションを開始
    ///
    /// # Returns
    /// (送信チャンネル, 受信チャンネル) のタプル
    /// - 送信チャンネル: PCM音声データ（i16サンプル）を送信
    /// - 受信チャンネル: 文字起こし結果を受信
    pub async fn start_stream(
        &mut self,
    ) -> Result<(mpsc::Sender<Vec<i16>>, mpsc::Receiver<TranscriptResult>)> {
        // バッファサイズを大幅拡張
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use crate::flac_encoder::FlacEncoder;

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<i16>>(4096);
        let audio_rx = Arc::new(Mutex::new(audio_rx));
        let (result_tx, result_rx) = mpsc::channel::<TranscriptResult>(32);

        // AWS SDKクライアント初期化
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = AwsTranscribeClient::new(&config);

        let language_code = match self.config.language_code.as_str() {
            "ja-JP" => LanguageCode::JaJp,
            "en-US" => LanguageCode::EnUs,
            other => LanguageCode::from(other),
        };
        let sample_rate = self.config.sample_rate;
        let channel_id = self.channel_id;
        let start_time = self.start_time;
        tokio::spawn({
            let language_code = language_code.clone();
            let sample_rate = sample_rate;
            let channel_id = channel_id;
            let start_time = start_time;
            let audio_rx = Arc::clone(&audio_rx);
            let client = client.clone();
            let result_tx = result_tx.clone();
            async move {
                use tokio::time::{Duration, timeout};
                'outer: loop {
                    let audio_rx_for_stream = Arc::clone(&audio_rx);

                    // FLACエンコーダーを作成（圧縮レベル5）
                    let mut flac_encoder = FlacEncoder::new(sample_rate, 5);

                    let input_stream = stream! {
                        let mut pcm_buffer: Vec<i16> = Vec::new();
                        let max_samples = 8000; // 約0.5秒分のサンプル（16kHzの場合）

                        loop {
                            let mut rx = audio_rx_for_stream.lock().await;

                            // データを待機（最大1秒）
                            match timeout(Duration::from_secs(1), rx.recv()).await {
                                Ok(Some(samples)) => {
                                    pcm_buffer.extend_from_slice(&samples);

                                    // バッファが一定サイズに達したらFLACエンコードして送信
                                    if pcm_buffer.len() >= max_samples {
                                        let to_encode: Vec<i16> = pcm_buffer.drain(..max_samples).collect();

                                        match flac_encoder.encode(&to_encode) {
                                            Ok(flac_data) => {
                                                let blob = Blob::new(flac_data);
                                                yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(blob).build()));
                                            }
                                            Err(e) => {
                                                log::error!("FLACエンコードエラー: {:?}", e);
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    log::debug!("TranscribeClient: チャンネルクローズ");
                                    // チャンネルがクローズされた場合、残りのバッファを送信
                                    if !pcm_buffer.is_empty() {
                                        match flac_encoder.encode(&pcm_buffer) {
                                            Ok(flac_data) => {
                                                let blob = Blob::new(flac_data);
                                                log::debug!("Amazon Transcribe 最終送信: {} サンプル → {} バイト", pcm_buffer.len(), blob.as_ref().len());
                                                yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(blob).build()));
                                            }
                                            Err(e) => {
                                                log::error!("FLACエンコードエラー: {:?}", e);
                                            }
                                        }
                                    }
                                    break;
                                }
                                Err(_) => {
                                    log::debug!("TranscribeClient: タイムアウト（データなし）");
                                    // タイムアウトした場合、バッファに残っているデータを送信
                                    if !pcm_buffer.is_empty() {
                                        let to_encode = pcm_buffer.split_off(0);
                                        match flac_encoder.encode(&to_encode) {
                                            Ok(flac_data) => {
                                                let blob = Blob::new(flac_data);
                                                log::debug!("Amazon Transcribe タイムアウト送信: {} サンプル → {} バイト", to_encode.len(), blob.as_ref().len());
                                                yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(blob).build()));
                                            }
                                            Err(e) => {
                                                log::error!("FLACエンコードエラー: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    };
                    let mut resp = match client
                        .start_stream_transcription()
                        .language_code(language_code.clone())
                        .media_sample_rate_hertz(sample_rate as i32)
                        .media_encoding(MediaEncoding::Flac)
                        .audio_stream(input_stream.into())
                        .send()
                        .await
                    {
                        Ok(r) => {
                            log::debug!("Transcribe Output: {:?}", r);
                            r
                        }
                        Err(e) => {
                            log::error!("Transcribe API開始失敗: {:?}", e);
                            return;
                        }
                    };
                    while let Ok(Some(event)) = resp.transcript_result_stream.recv().await {
                        match event {
                            aws_sdk_transcribestreaming::types::TranscriptResultStream::TranscriptEvent(transcript_event) => {
                                if let Some(transcript) = transcript_event.transcript {
                                    for result in transcript.results.unwrap_or_default() {
                                        for alt in result.alternatives.unwrap_or_default() {
                                            let text = alt.transcript.unwrap_or_default();
                                            let is_partial = result.is_partial;
                                            let transcript = TranscriptResult::new(
                                                channel_id, text, is_partial, start_time,
                                            );
                                            if let Err(e) = result_tx.try_send(transcript) {
                                                log::warn!("Transcribe結果送信失敗: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                            other => {
                                log::debug!("Transcribeイベント: {:?}", other);
                            }
                        }
                    }
                    break 'outer;
                }
            }
        });

        Ok((audio_tx, result_rx))
    }

    /// チャンネルIDを取得
    pub fn channel_id(&self) -> usize {
        self.channel_id
    }

    /// リトライ回数を取得
    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_transcribe_client_creation() {
        let config = TranscribeConfig {
            region: "ap-northeast-1".to_string(),
            language_code: "ja-JP".to_string(),
            sample_rate: 16000,
            max_retries: 3,
            timeout_seconds: 10,
        };

        let result = TranscribeClient::new(config, 0).await;
        assert!(result.is_ok());
    }
}

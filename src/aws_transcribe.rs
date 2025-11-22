use crate::config::TranscribeConfig;
use crate::transcribe_backend::TranscribeBackend;
use crate::types::{Stability, TranscriptResult};
use anyhow::Result;
use async_trait::async_trait;
use aws_config;
use aws_sdk_transcribestreaming::Client as AwsTranscribeClient;
use aws_sdk_transcribestreaming::types::{AudioEvent, AudioStream, LanguageCode, MediaEncoding};
use aws_smithy_types::Blob;
use std::time::SystemTime;
use tokio::sync::mpsc;
use async_stream::stream;

/// AWS Transcribe Streaming API クライアント
pub struct AwsTranscribeBackend {
    config: TranscribeConfig,
    channel_id: usize,
    start_time: SystemTime,
    /// 再接続回数（メトリクス収集用）
    reconnection_count: u32,
    /// 現在実行中のタスクハンドル（リソースリーク防止用）
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl AwsTranscribeBackend {
    pub async fn new(config: TranscribeConfig, channel_id: usize, start_time: SystemTime) -> Result<Self> {
        let start_time_debug = start_time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        log::info!(
            "チャンネル {}: start_time = {} (UNIX timestamp)",
            channel_id,
            start_time_debug
        );
        Ok(Self {
            config,
            channel_id,
            start_time,
            reconnection_count: 0,
            task_handle: None,
        })
    }
}

#[async_trait]
impl TranscribeBackend for AwsTranscribeBackend {
    async fn start_stream(
        &mut self,
    ) -> Result<(mpsc::Sender<Vec<i16>>, mpsc::Receiver<TranscriptResult>)> {
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use crate::flac_encoder::FlacEncoder;

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<i16>>(4096);
        let audio_rx = Arc::new(Mutex::new(audio_rx));
        let (result_tx, result_rx) = mpsc::channel::<TranscriptResult>(32);

        // AWS SDKクライアント初期化（チャンネルごとに独立した設定で作成）
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = AwsTranscribeClient::new(&sdk_config);

        log::info!(
            "チャンネル {}: AWS Transcribe クライアントを作成",
            self.channel_id
        );

        let language_code = match self.config.language_code.as_str() {
            "ja-JP" => LanguageCode::JaJp,
            "en-US" => LanguageCode::EnUs,
            other => LanguageCode::from(other),
        };
        let sample_rate = self.config.sample_rate;
        let channel_id = self.channel_id;
        let start_time = self.start_time;

        // 古いタスクがあれば破棄（チャンネルクローズにより自動終了）
        if let Some(old_handle) = self.task_handle.take() {
            log::debug!("チャンネル {}: 古いTranscribeタスクを破棄", channel_id);
            // タスクハンドルをドロップすることで、バックグラウンドで終了させる
            drop(old_handle);
        }

        let handle = tokio::spawn({
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

                    // FLACエンコーダーを作成（圧縮レベル8 = 最高圧縮）
                    let mut flac_encoder = FlacEncoder::new(sample_rate, 8);

                    let input_stream = stream! {
                        let mut pcm_buffer: Vec<i16> = Vec::new();
                        // サンプルレートに応じた適切なバッファサイズを計算
                        let max_samples = (sample_rate as f64 * 0.2) as usize; // 0.2秒分
                        let initial_min_samples = (sample_rate as f64 * 0.15) as usize; // 0.15秒分（再接続直後）
                        let mut chunk_count = 0; // 送信チャンク数をカウント

                        log::info!("チャンネル {}: バッファサイズ設定 - 初期: {}サンプル({:.2}秒), 通常: {}サンプル({:.2}秒) @ {}Hz",
                                   channel_id, initial_min_samples, initial_min_samples as f64 / sample_rate as f64,
                                   max_samples, max_samples as f64 / sample_rate as f64, sample_rate);

                        loop {
                            let mut rx = audio_rx_for_stream.lock().await;

                            // データを待機（最大100ms）- AWS Transcribeへの迅速なデータ送信を優先
                            match timeout(Duration::from_millis(100), rx.recv()).await {
                                Ok(Some(samples)) => {
                                    pcm_buffer.extend_from_slice(&samples);

                                    // 適応的バッファリング戦略
                                    // - 最初の5チャンク: より小さいバッファで高速送信（AWS 20秒タイムアウト対策）
                                    // - それ以降: 通常バッファサイズで安定送信
                                    let min_samples = if chunk_count < 5 {
                                        initial_min_samples
                                    } else {
                                        max_samples
                                    };

                                    // バッファが一定サイズに達したらFLACエンコードして送信
                                    if pcm_buffer.len() >= min_samples {
                                        let to_encode: Vec<i16> = pcm_buffer.drain(..min_samples.min(pcm_buffer.len())).collect();
                                        chunk_count += 1;

                                        match flac_encoder.encode(&to_encode) {
                                            Ok(flac_data) => {
                                                let blob = Blob::new(flac_data);
                                                if chunk_count % 10 == 0 {
                                                    log::info!(
                                                        "チャンネル {}: AWS送信 チャンク#{} - {}サンプル → {}バイト",
                                                        channel_id,
                                                        chunk_count,
                                                        to_encode.len(),
                                                        blob.as_ref().len()
                                                    );
                                                }
                                                yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(blob).build()));
                                            }
                                            Err(e) => {
                                                log::error!("FLACエンコードエラー: {:?}", e);
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    log::debug!("AwsTranscribeBackend: チャンネルクローズ");
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
                                    log::debug!("AwsTranscribeBackend: タイムアウト（データなし）");
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

                    log::info!("チャンネル {}: Amazon Transcribe ストリーム開始...", channel_id);
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
                            log::info!(
                                "チャンネル {}: Amazon Transcribe ストリーム開始成功 [PID={}, netstatで接続を確認してください]",
                                channel_id,
                                std::process::id()
                            );
                            r
                        }
                        Err(e) => {
                            log::error!("チャンネル {}: Amazon Transcribe API開始失敗: {:?}", channel_id, e);
                            // エラーの詳細情報をログ出力
                            if let Some(service_err) = e.as_service_error() {
                                log::error!("チャンネル {}: サービスエラー詳細: {:?}", channel_id, service_err);
                            }
                            return;
                        }
                    };

                    let mut last_recv_time = SystemTime::now();

                    loop {
                        // 【切り分けポイント1】recv()呼び出し直前のタイムスタンプ
                        let before_recv = SystemTime::now();
                        let before_recv_elapsed = before_recv.duration_since(start_time).unwrap().as_secs_f64();
                        let interval = before_recv.duration_since(last_recv_time).unwrap().as_secs_f64();

                        match resp.transcript_result_stream.recv().await {
                            Ok(Some(event)) => {
                                // 【切り分けポイント2】recv()完了直後のタイムスタンプ
                                let after_recv = SystemTime::now();
                                let after_recv_elapsed = after_recv.duration_since(start_time).unwrap().as_secs_f64();
                                let recv_block_time = after_recv_elapsed - before_recv_elapsed;

                                match event {
                                aws_sdk_transcribestreaming::types::TranscriptResultStream::TranscriptEvent(transcript_event) => {
                                if let Some(transcript) = transcript_event.transcript {
                                    for result in transcript.results.unwrap_or_default() {
                                        for alt in result.alternatives.unwrap_or_default() {
                                            let text = alt.transcript.unwrap_or_default();
                                            let is_partial = result.is_partial;

                                            // stabilityを計算（stableフラグから推測）
                                            let stability = if is_partial {
                                                alt.items.as_ref().map(|items| {
                                                    let total = items.len();
                                                    if total == 0 {
                                                        return Stability::Low;
                                                    }

                                                    // stableなitemの割合を計算
                                                    let stable_count = items.iter()
                                                        .filter(|item| item.stable.unwrap_or(false))
                                                        .count();
                                                    let stable_ratio = stable_count as f64 / total as f64;

                                                    // 安定性を判定
                                                    if stable_ratio >= 0.8 {
                                                        Stability::High
                                                    } else if stable_ratio >= 0.4 {
                                                        Stability::Medium
                                                    } else {
                                                        Stability::Low
                                                    }
                                                })
                                            } else {
                                                None
                                            };

                                            // 【切り分けポイント2】AWS Transcribeの音声タイムスタンプを取得
                                            let audio_start_time = alt.items.as_ref()
                                                .and_then(|items| items.first())
                                                .map(|item| item.start_time);
                                            let audio_end_time = alt.items.as_ref()
                                                .and_then(|items| items.last())
                                                .map(|item| item.end_time);

                                            let transcript = if let Some(start_secs) = audio_start_time {
                                                // AWS Transcribe の実際の音声タイムスタンプを使用
                                                if !is_partial && !text.is_empty() {
                                                    // 【切り分けポイント3】AWS応答遅延を計算
                                                    let aws_latency = if let Some(end_secs) = audio_end_time {
                                                        after_recv_elapsed - end_secs
                                                    } else {
                                                        after_recv_elapsed - start_secs
                                                    };

                                                    // 【切り分けポイント4】recv()ループの間隔をログ出力
                                                    if interval >= 1.0 {
                                                        log::warn!(
                                                            "チャンネル {}: recv()インターバルが長い！ interval={:.2}秒",
                                                            channel_id,
                                                            interval
                                                        );
                                                    }

                                                    log::info!(
                                                        "チャンネル {}: AWS応答受信 - interval={:.2}秒, before_recv={:.2}秒, after_recv={:.2}秒, recv_block={:.2}秒, audio_start={:.2}秒, audio_end={:.2}秒, AWS遅延={:.2}秒, text='{}'",
                                                        channel_id,
                                                        interval,
                                                        before_recv_elapsed,
                                                        after_recv_elapsed,
                                                        recv_block_time,
                                                        start_secs,
                                                        audio_end_time.unwrap_or(start_secs),
                                                        aws_latency,
                                                        text.chars().take(30).collect::<String>()
                                                    );
                                                }
                                                TranscriptResult::new_with_audio_time(
                                                    channel_id, text, is_partial, stability, start_secs,
                                                )
                                            } else {
                                                // start_time が取得できない場合は従来の方法
                                                if !is_partial && !text.is_empty() {
                                                    log::info!(
                                                        "チャンネル {}: AWS応答受信 - before_recv={:.2}秒, after_recv={:.2}秒, recv_block={:.2}秒 (fallback), text='{}'",
                                                        channel_id,
                                                        before_recv_elapsed,
                                                        after_recv_elapsed,
                                                        recv_block_time,
                                                        text.chars().take(30).collect::<String>()
                                                    );
                                                }
                                                TranscriptResult::new(
                                                    channel_id, text, is_partial, stability, start_time,
                                                )
                                            };
                                            if let Err(e) = result_tx.try_send(transcript) {
                                                log::warn!("Amazon Transcribe 結果送信失敗: {}", e);
                                            }
                                        }
                                    }
                                }
                                },
                                other => {
                                    log::warn!("チャンネル {}: Amazon Transcribe 未処理イベント: {:?}", channel_id, other);
                                }
                            }
                                // recv()完了後、次のループのためにタイムスタンプを更新
                                last_recv_time = after_recv;
                            },
                            Ok(None) => {
                                log::warn!("チャンネル {}: Amazon Transcribeストリームが予期せず終了（Ok(None)）", channel_id);
                                break 'outer;
                            },
                            Err(e) => {
                                log::error!("チャンネル {}: Amazon Transcribeストリーム受信エラー: {:?}", channel_id, e);
                                // エラーの詳細をログ出力
                                log::error!("チャンネル {}: エラー種別: {}", channel_id, std::any::type_name_of_val(&e));
                                break 'outer;
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TranscribeBackendType;

    #[tokio::test]
    async fn test_aws_transcribe_backend_creation() {
        let config = TranscribeConfig {
            backend: TranscribeBackendType::Aws,
            region: "ap-northeast-1".to_string(),
            language_code: "ja-JP".to_string(),
            sample_rate: 16000,
            max_retries: 3,
            timeout_seconds: 10,
            connect_on_startup: false,
            send_buffered_on_reconnect: true,
        };

        let start_time = SystemTime::now();
        let result = AwsTranscribeBackend::new(config, 0, start_time).await;
        assert!(result.is_ok());
    }
}

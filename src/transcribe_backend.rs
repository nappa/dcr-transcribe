use crate::types::TranscriptResult;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// 文字起こしバックエンドの共通トレイト
#[async_trait]
pub trait TranscribeBackend: Send {
    /// ストリーミング文字起こしセッションを開始
    ///
    /// # Returns
    /// (送信チャンネル, 受信チャンネル) のタプル
    /// - 送信チャンネル: PCM音声データ（i16サンプル）を送信
    /// - 受信チャンネル: 文字起こし結果を受信
    async fn start_stream(
        &mut self,
    ) -> Result<(mpsc::Sender<Vec<i16>>, mpsc::Receiver<TranscriptResult>)>;

    /// チャンネルIDを取得
    fn channel_id(&self) -> usize;

    /// start_timeをリセット（再接続時のタイムスタンプドリフト防止）
    fn reset_start_time(&mut self);
}

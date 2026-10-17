// trait の channel_id() メソッドは現時点で呼び出し元なし、将来用に保持
#![allow(dead_code)]

use crate::types::TranscriptResult;
use anyhow::Result;
use async_trait::async_trait;
use std::time::SystemTime;
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

    /// 次に開始するストリームの先頭サンプルが録音された時刻を設定する。
    ///
    /// 音声の再生位置でタイムスタンプを返すバックエンド(Amazon Transcribe)は、
    /// これを使ってストリーム内の秒数を `start_time` からの秒数に直す。
    fn set_stream_origin(&mut self, _origin: SystemTime) {}

    /// チャンネルIDを取得
    fn channel_id(&self) -> usize;
}

use crate::types::VadState;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Transcribe接続状態
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscribeStatus {
    /// 正常
    Connected,
    /// エラー
    Error,
    /// 無通信
    Disconnected,
}

/// 文字起こし結果（TUI表示用）
#[derive(Clone, Debug)]
pub struct TranscriptEntry {
    /// 文字起こしテキスト
    pub text: String,
    /// 時刻（時:分）
    pub time: String,
    /// 秒
    pub seconds: f64,
}

/// チャンネル状態（TUI表示用）
#[derive(Clone, Debug)]
pub struct ChannelState {
    /// チャンネルID
    pub channel_id: usize,
    /// チャンネル名
    pub channel_name: String,
    /// リアルタイムボリューム (dB)
    pub current_volume_db: f32,
    /// VAD閾値 (dB)
    pub vad_threshold_db: f32,
    /// VAD状態
    pub vad_state: VadState,
    /// 無音開始時刻（Silenceの場合のみ有効）
    silence_start: Option<Instant>,
    /// Transcribe接続状態
    pub transcribe_status: TranscribeStatus,
    /// 最新の文字起こし結果（最大10件）
    pub transcripts: VecDeque<TranscriptEntry>,
}

impl ChannelState {
    pub fn new(channel_id: usize, channel_name: String) -> Self {
        Self {
            channel_id,
            channel_name,
            current_volume_db: -100.0,
            vad_threshold_db: -40.0, // デフォルト値
            vad_state: VadState::Silence,
            silence_start: Some(Instant::now()),
            transcribe_status: TranscribeStatus::Disconnected,
            transcripts: VecDeque::new(),
        }
    }

    /// VAD閾値を設定
    pub fn set_vad_threshold(&mut self, threshold_db: f32) {
        self.vad_threshold_db = threshold_db;
    }

    /// リアルタイムボリュームを更新
    pub fn update_volume(&mut self, volume_db: f32) {
        self.current_volume_db = volume_db;
    }

    /// VAD状態を更新
    pub fn update_vad_state(&mut self, state: VadState) {
        // 状態が変わった場合のみ処理
        let state_changed = match (&self.vad_state, &state) {
            (VadState::Silence, VadState::Voice { .. }) => true,
            (VadState::Voice { .. }, VadState::Silence) => true,
            _ => false,
        };

        if state_changed {
            match state {
                VadState::Silence => {
                    // 無音に変わった時、開始時刻を記録
                    self.silence_start = Some(Instant::now());
                }
                VadState::Voice { .. } => {
                    // 音声に変わった時、無音開始時刻をクリア
                    self.silence_start = None;
                }
            }
        }

        self.vad_state = state;
    }

    /// 無音の持続時間を取得（秒）
    pub fn silence_duration_secs(&self) -> Option<f64> {
        match self.vad_state {
            VadState::Silence => {
                self.silence_start.map(|start| start.elapsed().as_secs_f64())
            }
            VadState::Voice { .. } => None,
        }
    }

    /// Transcribe接続状態を更新
    pub fn update_transcribe_status(&mut self, status: TranscribeStatus) {
        self.transcribe_status = status;
    }

    /// 文字起こし結果を追加
    pub fn add_transcript(&mut self, text: String, time: String, seconds: f64) {
        self.transcripts.push_back(TranscriptEntry {
            text,
            time,
            seconds,
        });

        // 最大10件まで保持
        while self.transcripts.len() > 10 {
            self.transcripts.pop_front();
        }
    }
}

/// 全チャンネルの状態を管理
#[derive(Clone)]
pub struct TuiState {
    channels: Arc<Mutex<Vec<ChannelState>>>,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// チャンネルを追加
    pub fn add_channel(&self, channel_id: usize, channel_name: String) {
        let mut channels = self.channels.lock().unwrap();
        channels.push(ChannelState::new(channel_id, channel_name));
    }

    /// チャンネル状態を取得
    pub fn get_channel(&self, channel_id: usize) -> Option<ChannelState> {
        let channels = self.channels.lock().unwrap();
        channels.iter().find(|c| c.channel_id == channel_id).cloned()
    }

    /// 全チャンネル状態を取得
    pub fn get_all_channels(&self) -> Vec<ChannelState> {
        let channels = self.channels.lock().unwrap();
        channels.clone()
    }

    /// チャンネル状態を更新
    pub fn update_channel<F>(&self, channel_id: usize, f: F)
    where
        F: FnOnce(&mut ChannelState),
    {
        let mut channels = self.channels.lock().unwrap();
        if let Some(channel) = channels.iter_mut().find(|c| c.channel_id == channel_id) {
            f(channel);
        }
    }
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new()
    }
}

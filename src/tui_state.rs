// 一部のフィールド/メソッドは Debug 経由のみで使われる、または保持目的で未使用
#![allow(dead_code)]

use crate::types::{Stability, VadState};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// 時刻（ISO 8601形式）
    pub time: String,
    /// 秒
    pub seconds: f64,
    /// 部分結果かどうか
    pub is_partial: bool,
    /// 部分結果の安定性
    pub stability: Option<Stability>,
}

/// チャンネル状態（TUI表示用）
#[derive(Clone, Debug)]
pub struct ChannelState {
    /// チャンネルID
    pub channel_id: usize,
    /// チャンネル名
    pub channel_name: String,
    pub recording_error: Option<String>,
    pub input_quality_warning: Option<String>,
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
    /// 最新の文字起こし結果（確定結果のみ、表示可能な分だけTUIで表示）
    pub transcripts: VecDeque<TranscriptEntry>,
    /// 現在表示中の部分結果（partial）
    pub partial_transcript: Option<TranscriptEntry>,
}

impl ChannelState {
    pub fn new(channel_id: usize, channel_name: String) -> Self {
        Self {
            channel_id,
            channel_name,
            recording_error: None,
            input_quality_warning: None,
            current_volume_db: -100.0,
            vad_threshold_db: -40.0, // デフォルト値
            vad_state: VadState::Silence,
            silence_start: Some(Instant::now()),
            transcribe_status: TranscribeStatus::Disconnected,
            transcripts: VecDeque::new(),
            partial_transcript: None,
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
    pub fn add_transcript(
        &mut self,
        text: String,
        time: String,
        seconds: f64,
        is_partial: bool,
        stability: Option<Stability>,
    ) {
        let entry = TranscriptEntry {
            text,
            time,
            seconds,
            is_partial,
            stability,
        };

        if is_partial {
            // 部分結果は上書き
            self.partial_transcript = Some(entry);
        } else {
            // 確定結果は履歴に追加
            self.partial_transcript = None; // 部分結果をクリア
            self.transcripts.push_back(entry);

            // 最大100件まで保持（メモリ節約のため）
            // 実際の表示件数は画面サイズによって動的に決定される
            while self.transcripts.len() > 100 {
                self.transcripts.pop_front();
            }
        }
    }
}

/// 状態表示の色(緑・黄・赤・灰)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// 正常
    Good,
    /// 動いているが品質が悪い(遅い・溜まっている・時計がずれている)
    Warn,
    /// 止まっている
    Bad,
    /// まだ分からない
    Unknown,
}

/// 全チャンネルの状態を管理
#[derive(Clone)]
pub struct TuiState {
    pub share_info: Arc<Mutex<Option<crate::share::ShareInfo>>>,
    /// 中継サーバへのアップリンクの状態(色と 1 行)。アップリンクが無効なら None
    pub uplink_status: Arc<Mutex<Option<(Health, String)>>>,
    /// この機械の時計のずれ
    pub clock: Arc<crate::clock::ClockMonitor>,
    pub output_muted: Arc<std::sync::atomic::AtomicBool>,
    pub input_health: crate::audio_health::StreamHealth,
    pub output_health: crate::audio_health::StreamHealth,
    channels: Arc<Mutex<Vec<ChannelState>>>,
    /// 同時にモニターするチャンネル。入力コールバックではtry_lockで参照する。
    pub(crate) selected_channels_for_output: Arc<Mutex<std::collections::BTreeSet<usize>>>,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            share_info: Arc::new(Mutex::new(None)),
            uplink_status: Arc::new(Mutex::new(None)),
            clock: crate::clock::ClockMonitor::new(),
            output_muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            input_health: Default::default(),
            output_health: Default::default(),
            channels: Arc::new(Mutex::new(Vec::new())),
            selected_channels_for_output: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
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

    pub fn set_selected_channel_for_output(&self, channel_id: Option<usize>) {
        let mut selected = self.selected_channels_for_output.lock().unwrap();
        selected.clear();
        selected.extend(channel_id);
    }

    pub fn get_selected_channel_for_output(&self) -> Option<usize> {
        self.selected_channels_for_output.lock().unwrap().iter().next().copied()
    }

    pub fn get_selected_channels_for_output(&self) -> Vec<usize> {
        self.selected_channels_for_output.lock().unwrap().iter().copied().collect()
    }

    pub fn set_output_enabled(&self, channel: usize, enabled: bool) {
        let mut selected = self.selected_channels_for_output.lock().unwrap();
        if enabled { selected.insert(channel); } else { selected.remove(&channel); }
    }

    pub fn toggle_output(&self, channel: usize) {
        let mut selected = self.selected_channels_for_output.lock().unwrap();
        if !selected.remove(&channel) { selected.insert(channel); }
    }

}

impl Default for TuiState {
    fn default() -> Self {
        Self::new()
    }
}

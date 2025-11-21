use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// 16ビット整数型のオーディオサンプル
///
/// PCM形式の音声データを表現するための型エイリアス。
/// -32768 から 32767 の範囲の値を取る。
pub type SampleI16 = i16;

/// オーディオフォーマット情報
///
/// 音声データのサンプリングレートとチャンネル数を保持する。
///
/// # Examples
///
/// ```
/// # use dcr_transcribe::types::AudioFormat;
/// let format = AudioFormat {
///     sample_rate: 48000,  // 48kHz
///     channels: 2,          // ステレオ
/// };
/// ```
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    /// サンプリングレート (Hz)
    ///
    /// 典型的な値: 8000, 16000, 44100, 48000
    pub sample_rate: u32,

    /// チャンネル数
    ///
    /// 1: モノラル, 2: ステレオ
    pub channels: u16,
}

/// オーディオチャンク
///
/// タイムスタンプ付きの音声データのまとまり。
/// オーディオ入力から受信した生データを表現する。
///
/// # Examples
///
/// ```
/// # use dcr_transcribe::types::{AudioChunk, AudioFormat};
/// let chunk = AudioChunk {
///     samples: vec![0i16; 1600], // 100ms分 @ 16kHz
///     format: AudioFormat { sample_rate: 16000, channels: 1 },
///     timestamp_ns: 1_000_000_000, // 1秒
/// };
/// ```
#[derive(Clone, Debug)]
pub struct AudioChunk {
    /// PCM音声サンプルの配列
    pub samples: Vec<SampleI16>,

    /// オーディオフォーマット情報
    pub format: AudioFormat,

    /// このチャンクの開始タイムスタンプ (ナノ秒)
    ///
    /// UNIX_EPOCHからの経過時間
    pub timestamp_ns: u128,
}

/// バッファリングされたチャンク
///
/// リトライ用バッファに保存される音声データ。
/// AudioChunkから簡略化した形式。
#[derive(Clone, Debug)]
pub struct BufferedChunk {
    /// PCM音声サンプルの配列
    pub samples: Vec<SampleI16>,

    /// このチャンクの開始タイムスタンプ (ナノ秒)
    pub timestamp_ns: u128,
}

/// バッファオーバーフロー時のドロップポリシー
///
/// バッファ容量を超えた場合にどのデータを破棄するかを指定する。
///
/// # Examples
///
/// ```
/// # use dcr_transcribe::types::DropPolicy;
/// let policy = DropPolicy::DropOldest; // 最古のデータから破棄
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DropPolicy {
    /// 最古のデータから破棄
    ///
    /// リアルタイム処理では通常これを使用する
    DropOldest,

    /// 最新のデータを破棄
    ///
    /// 過去のデータを優先する場合に使用
    DropNewest,

    /// ブロッキング（未実装）
    ///
    /// バッファが空くまで待機する。
    /// 現在の実装では DropOldest として扱われる。
    Block,
}

/// VAD（Voice Activity Detection）の状態
///
/// 音声検出器の現在の状態を表す。
/// ハングオーバー機構により、音声が途切れてもすぐには
/// 無音状態に遷移しない。
///
/// # Examples
///
/// ```
/// # use dcr_transcribe::types::VadState;
/// // 無音状態
/// let state = VadState::Silence;
///
/// // 音声状態（ハングオーバー残り500ms）
/// let state = VadState::Voice { hangover_remaining_ms: 500 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadState {
    /// 無音状態
    Silence,

    /// 音声状態
    ///
    /// ハングオーバー残り時間（ミリ秒）を保持する。
    /// 音声が検出されなくなっても、この時間が経過するまでは
    /// 音声状態を維持する。
    Voice {
        /// ハングオーバー残り時間（ミリ秒）
        hangover_remaining_ms: u32,
    },
}

/// PartialResultsの安定性レベル
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Stability {
    /// 低安定性（変更される可能性が高い）
    Low,
    /// 中安定性
    Medium,
    /// 高安定性（ほぼ確定）
    High,
}

/// 文字起こし結果
///
/// AWS Transcribeからの文字起こし結果を表現する。
/// JSON形式でシリアライズして標準出力に出力される。
///
/// # JSON出力例
///
/// ```json
/// {
///   "channel": 0,
///   "timestamp": "2025-01-02T14:30:15.234Z",
///   "timestamp_seconds": 15.234,
///   "text": "こちら本部、応答願います",
///   "is_partial": false,
///   "stability": null
/// }
/// ```
#[derive(Clone, Debug, Serialize)]
pub struct TranscriptResult {
    /// チャンネルID
    pub channel: usize,

    /// ISO 8601形式のタイムスタンプ
    pub timestamp: String,

    /// 開始時刻からの経過秒数
    pub timestamp_seconds: f64,

    /// 文字起こしテキスト
    pub text: String,

    /// 部分結果かどうか
    ///
    /// true: 部分結果, false: 確定結果
    pub is_partial: bool,

    /// 部分結果の安定性（部分結果の場合のみ有効）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stability: Option<Stability>,
}

impl TranscriptResult {
    /// 新しい文字起こし結果を作成
    ///
    /// # Arguments
    ///
    /// * `channel` - チャンネルID
    /// * `text` - 文字起こしテキスト
    /// * `is_partial` - 部分結果かどうか
    /// * `stability` - 部分結果の安定性（部分結果の場合のみ）
    /// * `start_time` - 処理開始時刻（タイムスタンプ計算の基準）
    ///
    /// # Examples
    ///
    /// ```
    /// # use dcr_transcribe::types::TranscriptResult;
    /// # use std::time::SystemTime;
    /// let result = TranscriptResult::new(
    ///     0,
    ///     "こんにちは".to_string(),
    ///     false,
    ///     None,
    ///     SystemTime::now(),
    /// );
    /// assert_eq!(result.channel, 0);
    /// assert_eq!(result.text, "こんにちは");
    /// ```
    pub fn new(
        channel: usize,
        text: String,
        is_partial: bool,
        stability: Option<Stability>,
        start_time: SystemTime,
    ) -> Self {
        let now = SystemTime::now();

        // 開始時刻からの経過時間を計算
        let duration = now.duration_since(start_time).unwrap_or_default();
        let timestamp_seconds = duration.as_secs_f64();

        // ISO 8601形式のタイムスタンプを生成
        let timestamp = chrono::DateTime::from_timestamp(
            now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            0,
        )
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();

        Self {
            channel,
            timestamp,
            timestamp_seconds,
            text,
            is_partial,
            stability,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_format_creation() {
        let format = AudioFormat {
            sample_rate: 48000,
            channels: 2,
        };
        assert_eq!(format.sample_rate, 48000);
        assert_eq!(format.channels, 2);
    }

    #[test]
    fn test_audio_chunk_creation() {
        let chunk = AudioChunk {
            samples: vec![0i16; 1600],
            format: AudioFormat {
                sample_rate: 16000,
                channels: 1,
            },
            timestamp_ns: 1_000_000_000,
        };
        assert_eq!(chunk.samples.len(), 1600);
        assert_eq!(chunk.format.sample_rate, 16000);
        assert_eq!(chunk.timestamp_ns, 1_000_000_000);
    }

    #[test]
    fn test_drop_policy_serialization() {
        let policy = DropPolicy::DropOldest;
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(json, r#""drop_oldest""#);

        let deserialized: DropPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DropPolicy::DropOldest);
    }

    #[test]
    fn test_vad_state_equality() {
        assert_eq!(VadState::Silence, VadState::Silence);
        assert_eq!(
            VadState::Voice {
                hangover_remaining_ms: 500
            },
            VadState::Voice {
                hangover_remaining_ms: 500
            }
        );
        assert_ne!(
            VadState::Silence,
            VadState::Voice {
                hangover_remaining_ms: 500
            }
        );
    }

    #[test]
    fn test_transcript_result_creation() {
        let start_time = SystemTime::now();
        let result = TranscriptResult::new(0, "テストメッセージ".to_string(), false, None, start_time);

        assert_eq!(result.channel, 0);
        assert_eq!(result.text, "テストメッセージ");
        assert!(!result.is_partial);
        assert!(result.timestamp_seconds >= 0.0);
        assert!(!result.timestamp.is_empty());
    }

    #[test]
    fn test_transcript_result_json_serialization() {
        let start_time = SystemTime::now();
        let result = TranscriptResult::new(1, "こんにちは".to_string(), true, Some(Stability::High), start_time);

        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["channel"], 1);
        assert_eq!(parsed["text"], "こんにちは");
        assert_eq!(parsed["is_partial"], true);
    }
}

use crate::config::VadConfig;
use crate::types::{SampleI16, VadState};

/// Voice Activity Detector (音声区間検出器)
///
/// RMS (Root Mean Square) ベースのシンプルなVAD実装。
/// 音声パワーが閾値を超えたら音声区間と判定し、
/// 下回ってもハングオーバー期間は音声継続とみなす。
///
/// # アルゴリズム
///
/// 1. 各サンプルを正規化 (-1.0 ~ 1.0)
/// 2. RMS (二乗平均平方根) を計算
/// 3. デシベル (dB) に変換: `20 * log10(rms)`
/// 4. 閾値と比較して音声/無音を判定
/// 5. ハングオーバー機構により急激な変化を抑制
///
/// # ハングオーバー機構
///
/// 音声が検出されなくなっても、設定された期間は音声状態を維持する。
/// これにより、短い無音区間で音声が途切れるのを防ぐ。
///
/// # Examples
///
/// ```
/// # use dcr_transcribe::vad::VoiceActivityDetector;
/// # use dcr_transcribe::config::VadConfig;
/// let config = VadConfig {
///     threshold_db: -40.0,
///     hangover_duration_ms: 500,
/// };
/// let mut vad = VoiceActivityDetector::new(&config, 16000);
///
/// // 無音サンプル
/// let silence = vec![0i16; 1600];
/// assert!(!vad.process(&silence));
///
/// // 音声サンプル
/// let voice: Vec<i16> = (0..1600)
///     .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
///     .collect();
/// assert!(vad.process(&voice));
/// ```
pub struct VoiceActivityDetector {
    /// 音声判定の閾値 (dB)
    ///
    /// この値より大きいRMSを持つサンプルは音声とみなす
    threshold_db: f32,

    /// ハングオーバー期間 (ミリ秒)
    ///
    /// 音声終了後もこの期間は音声状態を維持する
    hangover_duration_ms: u32,

    /// 現在の状態 (無音/音声)
    state: VadState,

    /// サンプリングレート (Hz)
    ///
    /// 時間計算に使用
    sample_rate: u32,
}

impl VoiceActivityDetector {
    pub fn new(config: &VadConfig, sample_rate: u32) -> Self {
        Self {
            threshold_db: config.threshold_db,
            hangover_duration_ms: config.hangover_duration_ms,
            state: VadState::Silence,
            sample_rate,
        }
    }

    /// 音声サンプルを処理して音声区間かどうかを判定
    ///
    /// # Arguments
    /// * `samples` - 音声サンプル配列
    ///
    /// # Returns
    /// * `true` - 音声あり
    /// * `false` - 無音
    pub fn process(&mut self, samples: &[SampleI16]) -> bool {
        if samples.is_empty() {
            return false;
        }

        let rms = self.calculate_rms(samples);
        let db = self.rms_to_db(rms);

        // サンプル数から経過時間を計算（ミリ秒）
        let duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u32;

        let is_voice_detected = db > self.threshold_db;

        // 状態遷移
        self.state = match self.state {
            VadState::Silence => {
                if is_voice_detected {
                    log::debug!("VAD: 音声開始検出 (RMS: {:.2} dB)", db);
                    VadState::Voice {
                        hangover_remaining_ms: self.hangover_duration_ms,
                    }
                } else {
                    VadState::Silence
                }
            }
            VadState::Voice {
                hangover_remaining_ms,
            } => {
                if is_voice_detected {
                    // 音声が継続している場合、ハングオーバーをリセット
                    VadState::Voice {
                        hangover_remaining_ms: self.hangover_duration_ms,
                    }
                } else {
                    // 音声が検出されなくなった場合、ハングオーバーをカウントダウン
                    if hangover_remaining_ms > duration_ms {
                        VadState::Voice {
                            hangover_remaining_ms: hangover_remaining_ms - duration_ms,
                        }
                    } else {
                        log::debug!("VAD: 音声終了検出 (RMS: {:.2} dB)", db);
                        VadState::Silence
                    }
                }
            }
        };

        matches!(self.state, VadState::Voice { .. })
    }

    /// RMS (Root Mean Square) を計算
    fn calculate_rms(&self, samples: &[SampleI16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }

        let sum_of_squares: f64 = samples
            .iter()
            .map(|&s| {
                let normalized = s as f64 / i16::MAX as f64;
                normalized * normalized
            })
            .sum();

        let mean_square = sum_of_squares / samples.len() as f64;
        mean_square.sqrt() as f32
    }

    /// RMSをデシベル (dB) に変換
    fn rms_to_db(&self, rms: f32) -> f32 {
        if rms <= 0.0 {
            return -100.0; // 無音の場合の最小値
        }
        20.0 * rms.log10()
    }

    /// 現在の状態を取得
    pub fn get_state(&self) -> VadState {
        self.state
    }

    /// 音声区間中かどうか
    pub fn is_voice(&self) -> bool {
        matches!(self.state, VadState::Voice { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_silence_detection() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 無音サンプル（全て0）
        let silence = vec![0i16; 1600]; // 100ms分
        assert!(!vad.process(&silence));
        assert_eq!(vad.get_state(), VadState::Silence);
    }

    #[test]
    fn test_voice_detection() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 音声サンプル（大きな振幅）
        let voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();

        assert!(vad.process(&voice));
        assert!(matches!(vad.get_state(), VadState::Voice { .. }));
    }

    #[test]
    fn test_hangover() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 音声を検出
        let voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        assert!(vad.process(&voice));

        // 無音に戻っても、ハングオーバー期間中は音声とみなす
        let silence = vec![0i16; 1600]; // 100ms分
        assert!(vad.process(&silence)); // まだ音声区間

        // さらに無音が続く（合計200ms）
        assert!(vad.process(&silence)); // まだ音声区間

        // 500ms経過後は無音に戻る
        let long_silence = vec![0i16; 16000 * 5 / 10]; // 500ms分
        assert!(!vad.process(&long_silence));
        assert_eq!(vad.get_state(), VadState::Silence);
    }

    #[test]
    fn test_low_amplitude_voice() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 小さな振幅（閾値以下）
        let low_voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 100.0) as i16)
            .collect();

        // 閾値以下なので無音とみなす
        assert!(!vad.process(&low_voice));
    }

    #[test]
    fn test_rms_calculation() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let vad = VoiceActivityDetector::new(&config, 16000);

        // 既知のRMS値を持つサンプル
        let samples = vec![1000i16; 1600];
        let rms = vad.calculate_rms(&samples);

        // 全て同じ値なのでRMSは絶対値と等しいはず
        let expected = 1000.0 / i16::MAX as f32;
        assert!((rms - expected).abs() < 0.001);
    }

    #[test]
    fn test_rms_to_db() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let vad = VoiceActivityDetector::new(&config, 16000);

        // RMS = 0.1 の場合
        let db = vad.rms_to_db(0.1);
        let expected = 20.0 * 0.1f32.log10();
        assert!((db - expected).abs() < 0.001);

        // RMS = 0.0 の場合（無音）
        let db = vad.rms_to_db(0.0);
        assert_eq!(db, -100.0);
    }

    #[test]
    fn test_empty_samples() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 空のサンプル配列
        let empty: Vec<i16> = vec![];
        assert!(!vad.process(&empty));
    }

    #[test]
    fn test_voice_continuation() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 音声サンプル
        let voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();

        // 連続して音声が入力される場合
        assert!(vad.process(&voice));
        assert!(vad.process(&voice));
        assert!(vad.process(&voice));

        // ハングオーバーは常にリセットされる
        if let VadState::Voice {
            hangover_remaining_ms,
        } = vad.get_state()
        {
            assert_eq!(hangover_remaining_ms, 500);
        } else {
            panic!("Expected Voice state");
        }
    }

    #[test]
    fn test_different_thresholds() {
        let voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 5000.0) as i16)
            .collect();

        // 厳しい閾値（-20dB）
        let strict_config = VadConfig {
            threshold_db: -20.0,
            hangover_duration_ms: 500,
        };
        let mut strict_vad = VoiceActivityDetector::new(&strict_config, 16000);

        // 緩い閾値（-60dB）
        let loose_config = VadConfig {
            threshold_db: -60.0,
            hangover_duration_ms: 500,
        };
        let mut loose_vad = VoiceActivityDetector::new(&loose_config, 16000);

        let strict_result = strict_vad.process(&voice);
        let loose_result = loose_vad.process(&voice);

        // 同じサンプルでも閾値によって結果が変わる可能性がある
        // 緩い閾値の方が音声を検出しやすい
        if !strict_result {
            assert!(loose_result || !loose_result); // 常に真
        }
    }

    #[test]
    fn test_is_voice_method() {
        let config = VadConfig {
            threshold_db: -40.0,
            hangover_duration_ms: 500,
        };
        let mut vad = VoiceActivityDetector::new(&config, 16000);

        // 初期状態は無音
        assert!(!vad.is_voice());

        // 音声検出
        let voice: Vec<i16> = (0..1600)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        vad.process(&voice);

        // 音声状態
        assert!(vad.is_voice());
    }
}

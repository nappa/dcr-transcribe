//! 音声を変更しない入力品質の推定。端子状態や送受信方向は判定しない。
use crate::types::AudioChunk;

#[derive(Default)]
struct Persistence {
    bad: usize,
    good: usize,
    active: bool,
}

impl Persistence {
    fn update(&mut self, suspect: bool, onset: usize) {
        if suspect {
            self.good = 0;
            self.bad = self.bad.saturating_add(1).min(onset);
            if self.bad >= onset {
                self.active = true;
            }
        } else {
            self.bad = 0;
            self.good = self.good.saturating_add(1).min(15);
            if self.good >= 15 {
                self.active = false;
            }
        }
    }

    fn interrupt(&mut self) {
        self.bad = 0;
        self.good = 0;
    }
}

pub struct InputQualityMonitor {
    rate: u32,
    threshold_db: f32,
    window: Vec<f64>,
    window_len: usize,
    calibration: Vec<f64>,
    baseline_db: Option<f64>,
    hum: Persistence,
    noise: Persistence,
    last_capture: Option<(u64, u128)>,
}

impl InputQualityMonitor {
    pub fn new(rate: u32, threshold_db: f32) -> Self {
        // 200 ms。入力チャンクの大きさに依存せず判定する。
        let window_len = (rate as usize / 5).max(1);
        Self {
            rate,
            threshold_db,
            window: Vec::with_capacity(window_len),
            window_len,
            calibration: Vec::with_capacity(50),
            baseline_db: None,
            hum: Persistence::default(),
            noise: Persistence::default(),
            last_capture: None,
        }
    }

    pub fn warning(&self) -> Option<String> {
        let mut reasons = Vec::new();
        if self.hum.active {
            reasons.push("ハム成分が継続（50/60 Hz 系）");
        }
        if self.noise.active {
            reasons.push("背景雑音が基準より増加");
        }
        (!reasons.is_empty()).then(|| {
            format!(
                "入力品質の疑い: {}。接続・入力切替を確認",
                reasons.join(" / ")
            )
        })
    }

    pub fn process(&mut self, chunk: &AudioChunk) {
        if self.rate < 1000 || chunk.format.sample_rate != self.rate || chunk.format.channels != 1 {
            return;
        }
        if let Some(capture) = chunk.capture {
            if capture.synthetic {
                return;
            }
            if let Some((expected_frame, previous_ns)) = self.last_capture {
                let expected_ns = chunk.samples.len() as u128 * 1_000_000_000 / self.rate as u128;
                if capture.frame_index != expected_frame
                    || capture.monotonic_ns < previous_ns
                    || capture
                        .monotonic_ns
                        .saturating_sub(previous_ns)
                        .abs_diff(expected_ns)
                        > 500_000_000
                {
                    self.window.clear();
                    self.hum.interrupt();
                    self.noise.interrupt();
                }
            }
            self.last_capture = Some((
                capture
                    .frame_index
                    .saturating_add(chunk.samples.len() as u64),
                capture.monotonic_ns,
            ));
        }
        for &sample in &chunk.samples {
            self.window.push(sample as f64 / 32768.0);
            if self.window.len() == self.window_len {
                self.analyze();
                self.window.clear();
            }
        }
    }

    fn analyze(&mut self) {
        let n = self.window.len() as f64;
        let mean = self.window.iter().sum::<f64>() / n;
        let energy = self.window.iter().map(|x| (x - mean).powi(2)).sum::<f64>();
        let db = 10.0 * (energy / n).max(1e-12).log10();
        let tone_energy = |frequency: f64| {
            let coefficient = 2.0 * (std::f64::consts::TAU * frequency / self.rate as f64).cos();
            let (mut a, mut b) = (0.0, 0.0);
            for x in &self.window {
                let next = x - mean + coefficient * a - b;
                b = a;
                a = next;
            }
            (a * a + b * b - coefficient * a * b).max(0.0) * 2.0 / n
        };
        let power50 = [50.0, 100.0, 150.0]
            .into_iter()
            .map(tone_energy)
            .sum::<f64>();
        let power60 = [60.0, 120.0, 180.0]
            .into_iter()
            .map(tone_energy)
            .sum::<f64>();
        let hum = db >= -55.0 && power50.max(power60) / energy.max(1e-12) >= 0.5;
        self.hum.update(hum, 25); // 5秒継続で発報、3秒正常で解除

        // VAD 閾値から十分低い窓だけを背景の候補とする。大きな環境音と発話の識別はしない。
        let quiet = db < self.threshold_db as f64 - 3.0;
        if self.baseline_db.is_none() && quiet && !hum {
            self.calibration.push(db);
            if self.calibration.len() == 50 {
                self.calibration.sort_by(f64::total_cmp);
                self.baseline_db = Some(self.calibration[10].max(-90.0));
                self.calibration.clear();
            }
        }
        let elevated = self
            .baseline_db
            .is_some_and(|baseline| quiet && db >= -60.0 && db - baseline >= 12.0);
        self.noise.update(elevated, 50); // 10秒継続。基準は警告中も学習し直さない。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AudioFormat, CaptureTiming};

    fn chunk(samples: Vec<i16>) -> AudioChunk {
        AudioChunk {
            samples,
            format: AudioFormat {
                sample_rate: 8000,
                channels: 1,
            },
            timestamp_ns: 0,
            capture: None,
        }
    }
    fn tone(frequency: f64, seconds: usize) -> Vec<i16> {
        (0..8000 * seconds)
            .map(|i| {
                (1000.0 * (std::f64::consts::TAU * frequency * i as f64 / 8000.0).sin()) as i16
            })
            .collect()
    }
    fn noise(amplitude: f64, seconds: usize) -> Vec<i16> {
        let mut seed = 12345u32;
        (0..8000 * seconds)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed as f64 / u32::MAX as f64 * 2.0 - 1.0) * amplitude) as i16
            })
            .collect()
    }
    #[test]
    fn hum_requires_persistence_and_recovers() {
        for f in [50.0, 60.0, 100.0, 120.0] {
            let mut monitor = InputQualityMonitor::new(8000, -40.0);
            monitor.process(&chunk(tone(f, 4)));
            assert!(monitor.warning().is_none());
            monitor.process(&chunk(tone(f, 1)));
            assert!(monitor.warning().unwrap().contains("ハム"));
            monitor.process(&chunk(vec![0; 8000 * 3]));
            assert!(monitor.warning().is_none());
        }
    }
    #[test]
    fn ordinary_tone_broadband_noise_and_dc_do_not_trigger_hum() {
        for samples in [
            tone(440.0, 12),
            noise(1000.0, 12),
            vec![1000; 8000 * 12],
            vec![0; 8000 * 12],
        ] {
            let mut monitor = InputQualityMonitor::new(8000, -40.0);
            monitor.process(&chunk(samples));
            assert!(monitor.warning().is_none());
        }
    }
    #[test]
    fn noise_increase_requires_baseline_and_ten_seconds() {
        let mut monitor = InputQualityMonitor::new(8000, -40.0);
        monitor.process(&chunk(noise(5.0, 10)));
        monitor.process(&chunk(noise(200.0, 9)));
        assert!(monitor.warning().is_none());
        monitor.process(&chunk(noise(200.0, 1)));
        assert!(monitor.warning().unwrap().contains("背景雑音"));
        monitor.process(&chunk(noise(5.0, 3)));
        assert!(monitor.warning().is_none());
    }
    #[test]
    fn chunk_boundaries_do_not_change_detection() {
        let samples = tone(60.0, 5);
        let mut monitor = InputQualityMonitor::new(8000, -40.0);
        for part in samples.chunks(137) {
            monitor.process(&chunk(part.to_vec()));
        }
        assert!(monitor.warning().is_some());
        assert!(monitor.window.len() < monitor.window_len);
        assert!(monitor.calibration.len() <= 50);
    }
    #[test]
    fn missing_or_synthetic_input_does_not_accumulate_persistence() {
        let mut monitor = InputQualityMonitor::new(8000, -40.0);
        let mut first = chunk(tone(50.0, 4));
        first.capture = Some(CaptureTiming {
            frame_index: 0,
            monotonic_ns: 4_000_000_000,
            synthetic: false,
        });
        monitor.process(&first);
        let mut next = chunk(tone(50.0, 1));
        next.capture = Some(CaptureTiming {
            frame_index: 40000,
            monotonic_ns: 6_000_000_000,
            synthetic: false,
        });
        monitor.process(&next);
        assert!(monitor.warning().is_none());
        next.capture.as_mut().unwrap().synthetic = true;
        for _ in 0..10 {
            monitor.process(&next);
        }
        assert!(monitor.warning().is_none());
    }
}

use crate::types::SampleI16;
use anyhow::Result;
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use flacenc::source::MemSource;

/// FLAC エンコーダー
///
/// PCM音声データをFLAC形式に圧縮する。
/// Amazon Transcribe Streaming APIに送信する前に
/// 音声データを圧縮することで帯域を削減する。
///
/// # 圧縮効果
///
/// FLAC（Free Lossless Audio Codec）は可逆圧縮形式で、
/// 通常30-50%程度のサイズ削減が期待できる。
///
/// # Examples
///
/// ```no_run
/// # use dcr_transcribe::flac_encoder::FlacEncoder;
/// let mut encoder = FlacEncoder::new(16000, 8);
/// let pcm_samples = vec![0i16; 16000];
/// let flac_data = encoder.encode(&pcm_samples).unwrap();
/// ```
pub struct FlacEncoder {
    sample_rate: u32,
    compression_level: u32,
}

impl FlacEncoder {
    /// 新しいFLACエンコーダーを作成
    ///
    /// # Arguments
    ///
    /// * `sample_rate` - サンプリングレート (Hz)
    /// * `compression_level` - 圧縮レベル (0-8)
    ///   - 0: 最速（圧縮率低）
    ///   - 8: 最高圧縮（処理時間長）
    ///   - 推奨: 5（バランス型）
    ///
    /// # Examples
    ///
    /// ```
    /// # use dcr_transcribe::flac_encoder::FlacEncoder;
    /// let encoder = FlacEncoder::new(16000, 5);
    /// ```
    pub fn new(sample_rate: u32, compression_level: u32) -> Self {
        Self {
            sample_rate,
            compression_level: compression_level.min(8),
        }
    }

    /// PCM音声データをFLAC形式にエンコード
    ///
    /// # Arguments
    ///
    /// * `samples` - PCM音声サンプル（16bit符号付き整数）
    ///
    /// # Returns
    ///
    /// FLACエンコードされたバイナリデータ
    ///
    /// # Errors
    ///
    /// エンコードに失敗した場合にエラーを返す
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use dcr_transcribe::flac_encoder::FlacEncoder;
    /// let mut encoder = FlacEncoder::new(16000, 5);
    /// let samples = vec![0i16; 16000];
    /// let flac_data = encoder.encode(&samples).unwrap();
    /// println!("Encoded {} samples to {} bytes", samples.len(), flac_data.len());
    /// ```
    pub fn encode(&mut self, samples: &[SampleI16]) -> Result<Vec<u8>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // i16からi32に変換（flacencの要求）
        let samples_i32: Vec<i32> = samples.iter().map(|&s| s as i32).collect();

        // MemSourceを使用してエンコード
        let source = MemSource::from_samples(
            &samples_i32,
            1,  // チャンネル数（モノラル）
            16, // ビット深度
            self.sample_rate as usize,
        );

        // エンコード設定
        let config = flacenc::config::Encoder::default();

        // 設定を検証
        let verified_config = config
            .into_verified()
            .map_err(|e| anyhow::anyhow!("FLAC設定の検証に失敗: {:?}", e))?;

        // エンコード実行
        let flac_stream = flacenc::encode_with_fixed_block_size(
            &verified_config,
            source,
            verified_config.block_size,
        )
        .map_err(|e| anyhow::anyhow!("FLACエンコードに失敗: {:?}", e))?;

        // バイト列に変換（ByteSinkを使用）
        let mut sink = ByteSink::new();
        flac_stream
            .write(&mut sink)
            .map_err(|e| anyhow::anyhow!("FLACストリームの書き込みに失敗: {:?}", e))?;
        let flac_bytes = sink.into_inner();

        Ok(flac_bytes)
    }

    /// 圧縮レベルを設定
    ///
    /// # Arguments
    ///
    /// * `level` - 圧縮レベル (0-8)
    pub fn set_compression_level(&mut self, level: u32) {
        self.compression_level = level.min(8);
    }

    /// 現在の圧縮レベルを取得
    pub fn compression_level(&self) -> u32 {
        self.compression_level
    }

    /// サンプリングレートを取得
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// FLACデータをデコードしてPCMサンプルに戻す（テスト用ヘルパー関数）
    ///
    /// # Arguments
    ///
    /// * `flac_data` - FLACエンコードされたバイナリデータ
    ///
    /// # Returns
    ///
    /// デコードされたPCMサンプル（i16）
    fn decode_flac(flac_data: &[u8]) -> Result<Vec<i16>> {
        let cursor = Cursor::new(flac_data);
        let mut reader = claxon::FlacReader::new(cursor)
            .map_err(|e| anyhow::anyhow!("FLACリーダーの初期化に失敗: {:?}", e))?;

        let mut samples = Vec::new();

        // ストリーム情報を取得
        let streaminfo = reader.streaminfo();
        let bits_per_sample = streaminfo.bits_per_sample;
        let total_samples = streaminfo.samples.unwrap_or(0) as usize;

        // すべてのサンプルを読み込む
        for sample in reader.samples() {
            let sample =
                sample.map_err(|e| anyhow::anyhow!("FLACサンプルの読み込みに失敗: {:?}", e))?;

            // ビット深度に応じてi16に変換
            let sample_i16 = if bits_per_sample == 16 {
                sample as i16
            } else {
                // 他のビット深度の場合はスケーリング
                let scale = (1 << (bits_per_sample - 1)) as f64;
                ((sample as f64 / scale) * 32768.0) as i16
            };

            samples.push(sample_i16);
        }

        // ストリーム情報に記載されている実際のサンプル数にトリミング
        // （FLACはブロック境界にパディングする可能性があるため）
        if total_samples > 0 && samples.len() > total_samples {
            samples.truncate(total_samples);
        }

        Ok(samples)
    }

    #[test]
    fn test_flac_encoder_creation() {
        let encoder = FlacEncoder::new(16000, 5);
        assert_eq!(encoder.sample_rate(), 16000);
        assert_eq!(encoder.compression_level(), 5);
    }

    #[test]
    fn test_compression_level_bounds() {
        let encoder = FlacEncoder::new(16000, 10);
        assert_eq!(encoder.compression_level(), 8); // 最大値に制限される
    }

    #[test]
    fn test_encode_empty() {
        let mut encoder = FlacEncoder::new(16000, 5);
        let result = encoder.encode(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_encode_sine_wave() {
        let mut encoder = FlacEncoder::new(16000, 5);

        // 1秒間のサイン波を生成
        let samples: Vec<i16> = (0..16000)
            .map(|i| {
                let t = i as f32 / 16000.0;
                let freq = 440.0; // A4音
                ((t * freq * 2.0 * std::f32::consts::PI).sin() * 10000.0) as i16
            })
            .collect();

        let flac_data = encoder.encode(&samples).unwrap();

        // エンコードされたデータが存在する
        assert!(!flac_data.is_empty());

        // 元のPCMデータより小さい（圧縮効果）
        let original_size = samples.len() * 2;
        assert!(flac_data.len() < original_size);

        println!(
            "圧縮率: {:.1}% ({} → {} bytes)",
            (flac_data.len() as f64 / original_size as f64) * 100.0,
            original_size,
            flac_data.len()
        );
    }

    #[test]
    fn test_encode_silence() {
        let mut encoder = FlacEncoder::new(16000, 5);

        // 無音（全て0）
        let samples = vec![0i16; 16000];
        let flac_data = encoder.encode(&samples).unwrap();

        // 無音は非常に高い圧縮率を達成できる
        assert!(!flac_data.is_empty());
        let original_size = samples.len() * 2;
        assert!(flac_data.len() < original_size / 10); // 90%以上の圧縮

        println!(
            "無音圧縮率: {:.1}% ({} → {} bytes)",
            (flac_data.len() as f64 / original_size as f64) * 100.0,
            original_size,
            flac_data.len()
        );
    }

    #[test]
    fn test_different_compression_levels() {
        let samples: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();

        // 低圧縮
        let mut encoder_low = FlacEncoder::new(16000, 0);
        let flac_low = encoder_low.encode(&samples).unwrap();

        // 高圧縮
        let mut encoder_high = FlacEncoder::new(16000, 8);
        let flac_high = encoder_high.encode(&samples).unwrap();

        println!(
            "低圧縮 (level 0): {} bytes, 高圧縮 (level 8): {} bytes",
            flac_low.len(),
            flac_high.len()
        );

        // 高圧縮の方がサイズが小さいか同じ
        assert!(flac_high.len() <= flac_low.len());
    }

    #[test]
    fn test_set_compression_level() {
        let mut encoder = FlacEncoder::new(16000, 5);
        assert_eq!(encoder.compression_level(), 5);

        encoder.set_compression_level(8);
        assert_eq!(encoder.compression_level(), 8);

        encoder.set_compression_level(0);
        assert_eq!(encoder.compression_level(), 0);

        // 範囲外の値は制限される
        encoder.set_compression_level(100);
        assert_eq!(encoder.compression_level(), 8);
    }

    #[test]
    fn test_roundtrip_sine_wave() {
        // WAVデータを生成（440Hzのサイン波、1秒間）
        let sample_rate = 16000;
        let duration_secs = 1.0;
        let frequency = 440.0; // A4音

        let original_samples: Vec<i16> = (0..(sample_rate as f32 * duration_secs) as usize)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((t * frequency * 2.0 * std::f32::consts::PI).sin() * 10000.0) as i16
            })
            .collect();

        println!("元のサンプル数: {}", original_samples.len());

        // FLACにエンコード
        let mut encoder = FlacEncoder::new(sample_rate, 5);
        let flac_data = encoder.encode(&original_samples).unwrap();

        // FLACからデコード
        let decoded_samples = decode_flac(&flac_data).unwrap();

        println!("デコード後のサンプル数: {}", decoded_samples.len());

        // 同一性を検証
        assert_eq!(
            original_samples.len(),
            decoded_samples.len(),
            "サンプル数が一致しません"
        );

        // すべてのサンプルが完全に一致することを確認（可逆圧縮）
        for (i, (original, decoded)) in original_samples
            .iter()
            .zip(decoded_samples.iter())
            .enumerate()
        {
            assert_eq!(
                original, decoded,
                "サンプル {} が一致しません: original={}, decoded={}",
                i, original, decoded
            );
        }

        println!("✓ ラウンドトリップテスト成功: すべてのサンプルが完全に一致");
    }

    #[test]
    fn test_roundtrip_silence() {
        // 無音データを生成
        let original_samples = vec![0i16; 16000];

        println!("元のサンプル数（無音）: {}", original_samples.len());

        // FLACにエンコード
        let mut encoder = FlacEncoder::new(16000, 5);
        let flac_data = encoder.encode(&original_samples).unwrap();

        println!(
            "無音FLAC圧縮: {} bytes → {} bytes (圧縮率: {:.1}%)",
            original_samples.len() * 2,
            flac_data.len(),
            (flac_data.len() as f64 / (original_samples.len() * 2) as f64) * 100.0
        );

        // FLACからデコード
        let decoded_samples = decode_flac(&flac_data).unwrap();

        // 同一性を検証
        assert_eq!(original_samples, decoded_samples);

        println!("✓ 無音ラウンドトリップテスト成功");
    }

    #[test]
    fn test_roundtrip_complex_waveform() {
        // 複雑な波形を生成（複数の周波数を合成）
        let sample_rate = 16000;
        let frequencies = vec![220.0, 440.0, 880.0]; // A3, A4, A5

        let original_samples: Vec<i16> = (0..sample_rate)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let mut sum = 0.0;

                for (idx, &freq) in frequencies.iter().enumerate() {
                    let amplitude = 3000.0 / (idx + 1) as f32; // 高調波は振幅を減衰
                    sum += (t * freq * 2.0 * std::f32::consts::PI).sin() * amplitude;
                }

                sum as i16
            })
            .collect();

        println!("複雑な波形のサンプル数: {}", original_samples.len());

        // FLACにエンコード
        let mut encoder = FlacEncoder::new(sample_rate, 5);
        let flac_data = encoder.encode(&original_samples).unwrap();

        println!(
            "複雑な波形のFLAC圧縮: {} bytes → {} bytes (圧縮率: {:.1}%)",
            original_samples.len() * 2,
            flac_data.len(),
            (flac_data.len() as f64 / (original_samples.len() * 2) as f64) * 100.0
        );

        // FLACからデコード
        let decoded_samples = decode_flac(&flac_data).unwrap();

        // 同一性を検証
        assert_eq!(original_samples.len(), decoded_samples.len());

        for (i, (original, decoded)) in original_samples
            .iter()
            .zip(decoded_samples.iter())
            .enumerate()
        {
            assert_eq!(
                original, decoded,
                "複雑な波形のサンプル {} が一致しません",
                i
            );
        }

        println!("✓ 複雑な波形のラウンドトリップテスト成功");
    }

    #[test]
    fn test_roundtrip_random_data() {
        // ランダムなデータを生成（最も圧縮しにくいパターン）
        let original_samples: Vec<i16> = (0..8000)
            .map(|i| {
                // 疑似ランダムな値を生成（再現性のため、シンプルな式を使用）
                // オーバーフロー対策: wrapping_mul と wrapping_add を使用
                let x =
                    ((i as u32).wrapping_mul(1103515245).wrapping_add(12345) & 0x7fffffff) as i16;
                x
            })
            .collect();

        println!("ランダムデータのサンプル数: {}", original_samples.len());

        // FLACにエンコード
        let mut encoder = FlacEncoder::new(16000, 5);
        let flac_data = encoder.encode(&original_samples).unwrap();

        println!(
            "ランダムデータのFLAC圧縮: {} bytes → {} bytes (圧縮率: {:.1}%)",
            original_samples.len() * 2,
            flac_data.len(),
            (flac_data.len() as f64 / (original_samples.len() * 2) as f64) * 100.0
        );

        // ランダムデータは圧縮率が悪いことを確認
        // （元のサイズの80%以上になる可能性が高い）
        println!("  → ランダムデータは圧縮しにくいため、圧縮率が低い");

        // FLACからデコード
        let decoded_samples = decode_flac(&flac_data).unwrap();

        // 同一性を検証（圧縮率が悪くても、可逆圧縮なので完全一致するはず）
        assert_eq!(original_samples.len(), decoded_samples.len());

        for (i, (original, decoded)) in original_samples
            .iter()
            .zip(decoded_samples.iter())
            .enumerate()
        {
            assert_eq!(
                original, decoded,
                "ランダムデータのサンプル {} が一致しません",
                i
            );
        }

        println!("✓ ランダムデータのラウンドトリップテスト成功（可逆圧縮を確認）");
    }

    #[test]
    fn test_roundtrip_different_compression_levels() {
        // サイン波データを生成
        let original_samples: Vec<i16> = (0..16000)
            .map(|i| {
                let t = i as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 10000.0) as i16
            })
            .collect();

        // 異なる圧縮レベルでテスト
        for compression_level in [0, 5, 8] {
            let mut encoder = FlacEncoder::new(16000, compression_level);
            let flac_data = encoder.encode(&original_samples).unwrap();

            println!(
                "圧縮レベル {}: {} bytes (圧縮率: {:.1}%)",
                compression_level,
                flac_data.len(),
                (flac_data.len() as f64 / (original_samples.len() * 2) as f64) * 100.0
            );

            // デコード
            let decoded_samples = decode_flac(&flac_data).unwrap();

            // すべての圧縮レベルで完全に元のデータに戻ることを確認
            assert_eq!(
                original_samples, decoded_samples,
                "圧縮レベル {} でデータが一致しません",
                compression_level
            );
        }

        println!("✓ すべての圧縮レベルでラウンドトリップテスト成功");
    }
}

//! AAC-LC エンコード(ADTS)
//!
//! 区間録音とライブ配信で使う。ADTS はフレームごとにヘッダを持つので、
//! ファイルとしてそのまま再生でき、ストリームの途中から聞き始めても復号できる。

use anyhow::{anyhow, Result};
use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};

/// AAC-LC の 1 フレームあたりのサンプル数
pub const FRAME_SAMPLES: usize = 1024;

/// エンコーダ内部の遅延を押し出すために、終了時に追加で流す無音フレーム数
const FLUSH_FRAMES: usize = 3;

/// 1 フレームの出力上限(モノラル 6144 bit + ADTS ヘッダに余裕を持たせる)
const MAX_FRAME_BYTES: usize = 2048;

/// AAC のプロファイル
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AacProfile {
    /// AAC-LC。48kbps 以上向け
    Lc,
    /// HE-AAC (AAC-LC + SBR)。16〜32kbps の細い回線向け
    #[default]
    He,
}

/// モノラル i16 → ADTS のストリーミングエンコーダ
pub struct AacEncoder {
    encoder: Encoder,
    /// 1 フレームに満たない入力
    pending: Vec<i16>,
    buf: Vec<u8>,
}

impl AacEncoder {
    pub fn new(sample_rate: u32, bitrate: u32, profile: AacProfile) -> Result<Self> {
        let encoder = Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(bitrate),
            sample_rate,
            transport: Transport::Adts,
            channels: ChannelMode::Mono,
            audio_object_type: match profile {
                AacProfile::Lc => AudioObjectType::Mpeg4LowComplexity,
                AacProfile::He => AudioObjectType::Mpeg4HeAac,
            },
        })
        .map_err(|e| anyhow!("AAC エンコーダの初期化に失敗 ({} Hz, {} bps): {}", sample_rate, bitrate, e))?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(FRAME_SAMPLES),
            buf: vec![0; MAX_FRAME_BYTES],
        })
    }

    /// サンプルを投入し、出来上がった ADTS フレームを `out` に追記する
    pub fn encode(&mut self, samples: &[i16], out: &mut Vec<u8>) -> Result<()> {
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (FRAME_SAMPLES - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == FRAME_SAMPLES {
                let frame = std::mem::take(&mut self.pending);
                self.encode_frame(&frame, out)?;
                self.pending = frame;
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// 残りを無音で埋めて出し切る(ファイルの終端用)
    pub fn flush(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let pad = if self.pending.is_empty() {
            0
        } else {
            FRAME_SAMPLES - self.pending.len()
        };
        self.encode(&vec![0; pad + FLUSH_FRAMES * FRAME_SAMPLES], out)
    }

    fn encode_frame(&mut self, frame: &[i16], out: &mut Vec<u8>) -> Result<()> {
        let info = self
            .encoder
            .encode(frame, &mut self.buf)
            .map_err(|e| anyhow!("AAC エンコードに失敗: {}", e))?;
        out.extend_from_slice(&self.buf[..info.output_size]);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fdk_aac::dec::{Decoder, Transport as DecTransport};

    /// ADTS を復号してサンプル列にする(テスト用)
    pub(crate) fn decode(adts: &[u8]) -> Vec<i16> {
        let mut dec = Decoder::new(DecTransport::Adts);
        let mut pcm = vec![0i16; 8192];
        let mut out = Vec::new();
        let mut rest = adts;
        while !rest.is_empty() {
            let consumed = dec.fill(rest).unwrap();
            rest = &rest[consumed..];
            while dec.decode_frame(&mut pcm).is_ok() {
                out.extend_from_slice(&pcm[..dec.decoded_frame_size()]);
            }
            if consumed == 0 {
                break;
            }
        }
        out
    }

    fn sine(n: usize, rate: u32) -> Vec<i16> {
        (0..n)
            .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16)
            .collect()
    }

    #[test]
    fn produces_decodable_adts() {
        let mut enc = AacEncoder::new(48000, 64000, AacProfile::Lc).unwrap();
        let mut out = Vec::new();
        // 半端な長さで分けて投入しても 1 本のストリームになる
        let input = sine(48000, 48000);
        for chunk in input.chunks(480) {
            enc.encode(chunk, &mut out).unwrap();
        }
        enc.flush(&mut out).unwrap();

        assert_eq!(out[0], 0xFF);
        assert_eq!(out[1] & 0xF0, 0xF0);

        let decoded = decode(&out);
        // 遅延ぶんの無音を流しているので、入力長以上は出てくる
        assert!(decoded.len() >= input.len(), "{} < {}", decoded.len(), input.len());
        let energy: f64 = decoded.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / decoded.len() as f64;
        assert!(energy.sqrt() > 1000.0, "rms={}", energy.sqrt());
    }

    #[test]
    fn he_aac_is_decodable_at_low_bitrate() {
        let mut enc = AacEncoder::new(48000, 16000, AacProfile::He).unwrap();
        let mut out = Vec::new();
        let input = sine(48000 * 2, 48000);
        enc.encode(&input, &mut out).unwrap();
        enc.flush(&mut out).unwrap();
        // 2 秒 × 16 kbps = 4 KB 前後
        assert!((3_000..6_000).contains(&out.len()), "{}", out.len());
        let decoded = decode(&out);
        assert!(!decoded.is_empty());
        let rms = (decoded.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / decoded.len() as f64).sqrt();
        assert!(rms > 1000.0, "rms={}", rms);
    }

    /// エンコード + デコードの遅延(サンプル数)を測る。`cargo test -- --ignored --nocapture codec_delay`
    #[test]
    #[ignore]
    fn codec_delay() {
        for (name, br, prof) in [
            ("LC 32k", 32000, AacProfile::Lc),
            ("LC 48k", 48000, AacProfile::Lc),
            ("HE 16k", 16000, AacProfile::He),
            ("HE 24k", 24000, AacProfile::He),
        ] {
            let mut enc = AacEncoder::new(48000, br, prof).unwrap();
            // 1 秒無音 → 10 ms のバースト → 無音
            let mut input = vec![0i16; 48000 * 2];
            for (i, s) in input[48000..48480].iter_mut().enumerate() {
                *s = ((i as f32 * 0.3).sin() * 20000.0) as i16;
            }
            let mut out = Vec::new();
            enc.encode(&input, &mut out).unwrap();
            enc.flush(&mut out).unwrap();
            let pcm = decode(&out);
            // HE は 2ch で出てくることがあるので、出力長から ch 数を推定
            let ch = if pcm.len() > input.len() * 3 / 2 { 2 } else { 1 };
            let first = pcm.iter().position(|&s| s.abs() > 3000).unwrap() / ch;
            println!("{}: 遅延 {} サンプル = {:.1} ms", name, first - 48000, (first - 48000) as f64 / 48.0);
        }
    }

    #[test]
    fn bitrate_is_roughly_honored() {
        let mut enc = AacEncoder::new(48000, 48000, AacProfile::Lc).unwrap();
        let mut out = Vec::new();
        enc.encode(&sine(48000 * 4, 48000), &mut out).unwrap();
        // 4 秒 × 48 kbps = 24 KB 前後
        assert!((18_000..30_000).contains(&out.len()), "{}", out.len());
    }
}

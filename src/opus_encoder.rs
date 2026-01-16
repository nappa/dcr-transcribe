use crate::types::SampleI16;
use anyhow::Result;
use ogg::writing::PacketWriter;
use opus::{Application, Channels, Encoder as OpusEncoderInner};
use std::io::Cursor;

/// OGG-Opus エンコーダー（ストリーミング対応）
///
/// PCM音声データをOGG-Opus形式に圧縮する。
/// Amazon Transcribe Streaming APIに送信する際に
/// 音声データを圧縮することで帯域を削減する。
///
/// # ストリーミングモード
///
/// このエンコーダーはストリーミング用に設計されている。
/// - 最初の `encode()` 呼び出しでOGGヘッダー（OpusHead/OpusTags）を含む
/// - 以降の呼び出しでは音声データのみをエンコード
/// - 各呼び出しで有効なOGGストリームセグメントを返す
///
/// # Examples
///
/// ```no_run
/// # use dcr_transcribe::opus_encoder::OpusEncoder;
/// let mut encoder = OpusEncoder::new(48000, 64000).unwrap();
/// let pcm_samples = vec![0i16; 48000];
/// let ogg_opus_data = encoder.encode(&pcm_samples).unwrap();
/// ```
pub struct OpusEncoder {
    opus_encoder: OpusEncoderInner,
    sample_rate: u32,
    bitrate: i32,
    frame_size: usize,
    granule_position: u64,
    serial_number: u32,
    header_written: bool,
    /// 累積ページシーケンス番号（OGGストリームの連続性を保つため）
    page_sequence_base: u32,
}

impl OpusEncoder {
    /// 新しいOGG-Opusエンコーダーを作成
    ///
    /// # Arguments
    ///
    /// * `sample_rate` - サンプリングレート (Hz)。Opusは48000Hzを推奨。
    ///   他のサンプルレートの場合は内部で変換される。
    /// * `bitrate` - ビットレート (bps)。音声の場合は24000〜64000を推奨。
    ///
    /// # Errors
    ///
    /// Opusエンコーダーの初期化に失敗した場合にエラーを返す。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use dcr_transcribe::opus_encoder::OpusEncoder;
    /// let encoder = OpusEncoder::new(48000, 64000).unwrap();
    /// ```
    pub fn new(sample_rate: u32, bitrate: i32) -> Result<Self> {
        // Opusは特定のサンプルレートのみサポート (8000, 12000, 16000, 24000, 48000)
        // サポートされていないサンプルレートは48000Hzにフォールバック
        let effective_sample_rate = match sample_rate {
            8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
            _ => {
                log::warn!(
                    "OpusEncoder: サポートされていないサンプルレート {}Hz。48000Hz として処理します。",
                    sample_rate
                );
                48000
            }
        };

        // フレームサイズ: 20ms (Opusの推奨フレームサイズ)
        // 20ms @ 48kHz = 960 samples
        let frame_size = (effective_sample_rate as usize * 20) / 1000;

        // Opusエンコーダーを作成
        let mut opus_encoder =
            OpusEncoderInner::new(effective_sample_rate, Channels::Mono, Application::Voip)
                .map_err(|e| anyhow::anyhow!("Opusエンコーダーの初期化に失敗: {:?}", e))?;

        opus_encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate))
            .map_err(|e| anyhow::anyhow!("ビットレートの設定に失敗: {:?}", e))?;

        Ok(Self {
            opus_encoder,
            sample_rate: effective_sample_rate,
            bitrate,
            frame_size,
            granule_position: 0,
            serial_number: rand_serial(),
            header_written: false,
            page_sequence_base: 0,
        })
    }

    /// PCM音声データをOGG-Opus形式にエンコード
    ///
    /// # Arguments
    ///
    /// * `samples` - PCM音声サンプル（16bit符号付き整数、モノラル）
    ///
    /// # Returns
    ///
    /// OGG-Opusエンコードされたバイナリデータ
    /// - 最初の呼び出し: OGGヘッダー + 音声データ
    /// - 以降の呼び出し: 音声データのみ
    ///
    /// # Errors
    ///
    /// エンコードに失敗した場合にエラーを返す
    pub fn encode(&mut self, samples: &[SampleI16]) -> Result<Vec<u8>> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // OGGコンテナに書き込み
        let mut ogg_buffer = Cursor::new(Vec::new());
        let mut packet_writer = PacketWriter::new(&mut ogg_buffer);

        // 最初の呼び出しではヘッダーを書き込む
        if !self.header_written {
            // OpusHead パケット（OGG-Opus仕様に準拠）
            let opus_head = create_opus_head(self.sample_rate);
            packet_writer
                .write_packet(
                    opus_head,
                    self.serial_number,
                    ogg::writing::PacketWriteEndInfo::EndPage,
                    0,
                )
                .map_err(|e| anyhow::anyhow!("OpusHead書き込みに失敗: {:?}", e))?;

            // OpusTags パケット
            let opus_tags = create_opus_tags();
            packet_writer
                .write_packet(
                    opus_tags,
                    self.serial_number,
                    ogg::writing::PacketWriteEndInfo::EndPage,
                    0,
                )
                .map_err(|e| anyhow::anyhow!("OpusTags書き込みに失敗: {:?}", e))?;

            self.header_written = true;
            log::debug!("OpusEncoder: OGGヘッダーを書き込みました");
        }

        // 音声データをフレーム単位でエンコード
        let mut output_buffer = vec![0u8; 4000]; // 最大エンコード出力サイズ
        let mut offset = 0;

        while offset < samples.len() {
            let remaining = samples.len() - offset;
            let frame_samples = remaining.min(self.frame_size);

            // フレームが足りない場合は0でパディング
            let frame: Vec<i16> = if frame_samples < self.frame_size {
                let mut padded = samples[offset..offset + frame_samples].to_vec();
                padded.resize(self.frame_size, 0);
                padded
            } else {
                samples[offset..offset + self.frame_size].to_vec()
            };

            let encoded_len = self
                .opus_encoder
                .encode(&frame, &mut output_buffer)
                .map_err(|e| anyhow::anyhow!("Opusエンコードに失敗: {:?}", e))?;

            self.granule_position += self.frame_size as u64;

            // EndPageを使用してページを強制出力（NormalPacketだとバッファリングされる）
            packet_writer
                .write_packet(
                    output_buffer[..encoded_len].to_vec(),
                    self.serial_number,
                    ogg::writing::PacketWriteEndInfo::EndPage,
                    self.granule_position,
                )
                .map_err(|e| anyhow::anyhow!("OGGパケット書き込みに失敗: {:?}", e))?;

            offset += self.frame_size;
        }

        let mut result = ogg_buffer.into_inner();

        // ページシーケンス番号をパッチして連続性を保つ
        let pages_in_this_call = patch_page_sequences(&mut result, self.page_sequence_base);
        self.page_sequence_base += pages_in_this_call;

        Ok(result)
    }

    /// ストリームを終了し、最終パケットを返す
    ///
    /// ストリーミング終了時に呼び出して、OGGストリームを適切に終了する
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        if !self.header_written {
            return Ok(Vec::new());
        }

        let mut ogg_buffer = Cursor::new(Vec::new());
        let mut packet_writer = PacketWriter::new(&mut ogg_buffer);

        // 無音フレームをEndStreamとして書き込み
        let silence = vec![0i16; self.frame_size];
        let mut output_buffer = vec![0u8; 4000];

        let encoded_len = self
            .opus_encoder
            .encode(&silence, &mut output_buffer)
            .map_err(|e| anyhow::anyhow!("Opusエンコードに失敗: {:?}", e))?;

        self.granule_position += self.frame_size as u64;

        packet_writer
            .write_packet(
                output_buffer[..encoded_len].to_vec(),
                self.serial_number,
                ogg::writing::PacketWriteEndInfo::EndStream,
                self.granule_position,
            )
            .map_err(|e| anyhow::anyhow!("OGGパケット書き込みに失敗: {:?}", e))?;

        Ok(ogg_buffer.into_inner())
    }

    /// エンコーダーをリセット（新しいストリームを開始）
    pub fn reset(&mut self) -> Result<()> {
        self.granule_position = 0;
        self.serial_number = rand_serial();
        self.header_written = false;
        self.page_sequence_base = 0;

        // Opusエンコーダーを再作成
        let mut opus_encoder =
            OpusEncoderInner::new(self.sample_rate, Channels::Mono, Application::Voip)
                .map_err(|e| anyhow::anyhow!("Opusエンコーダーの初期化に失敗: {:?}", e))?;

        opus_encoder
            .set_bitrate(opus::Bitrate::Bits(self.bitrate))
            .map_err(|e| anyhow::anyhow!("ビットレートの設定に失敗: {:?}", e))?;

        self.opus_encoder = opus_encoder;
        Ok(())
    }

    /// サンプリングレートを取得
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// ビットレートを取得
    pub fn bitrate(&self) -> i32 {
        self.bitrate
    }

    /// フレームサイズを取得
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}

/// OpusHead パケットを生成（RFC 7845準拠）
fn create_opus_head(sample_rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);

    // Magic signature
    head.extend_from_slice(b"OpusHead");

    // Version (1 byte)
    head.push(1);

    // Channel count (1 byte) - モノラル
    head.push(1);

    // Pre-skip (2 bytes, little-endian)
    // Opusエンコーダーのレイテンシ補正用（通常312サンプル @ 48kHz）
    let pre_skip: u16 = 312;
    head.extend_from_slice(&pre_skip.to_le_bytes());

    // Input sample rate (4 bytes, little-endian)
    head.extend_from_slice(&sample_rate.to_le_bytes());

    // Output gain (2 bytes, little-endian) - 0dB
    head.extend_from_slice(&0u16.to_le_bytes());

    // Channel mapping family (1 byte) - 0 = mono/stereo
    head.push(0);

    head
}

/// OpusTags パケットを生成（RFC 7845準拠）
fn create_opus_tags() -> Vec<u8> {
    let mut tags = Vec::with_capacity(60);

    // Magic signature
    tags.extend_from_slice(b"OpusTags");

    // Vendor string length (4 bytes, little-endian)
    let vendor = b"dcr-transcribe";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());

    // Vendor string
    tags.extend_from_slice(vendor);

    // User comment list length (4 bytes, little-endian) - 0 comments
    tags.extend_from_slice(&0u32.to_le_bytes());

    tags
}

/// ランダムなシリアル番号を生成
fn rand_serial() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    (now as u32) ^ 0xDEADBEEF
}

/// OGGページのシーケンス番号をパッチして連続性を保つ
///
/// PacketWriterは各インスタンスでページシーケンスを0からカウントするため、
/// 複数回のencode()呼び出しで生成されたデータを連結すると不正なOGGストリームになる。
/// この関数は出力データ内のすべてのOGGページを走査し、シーケンス番号を調整する。
///
/// # Arguments
/// * `data` - パッチするOGGデータ（in-place で変更）
/// * `base_sequence` - このデータの最初のページに使用するシーケンス番号
///
/// # Returns
/// このデータに含まれるページ数
fn patch_page_sequences(data: &mut [u8], base_sequence: u32) -> u32 {
    let mut page_count = 0u32;
    let mut pos = 0;

    while pos + 27 <= data.len() {
        // OggSマジックナンバーを探す
        if &data[pos..pos + 4] != b"OggS" {
            pos += 1;
            continue;
        }

        // OGGページ構造:
        // 0-3: "OggS"
        // 4: version (0)
        // 5: flags
        // 6-13: granule position (8 bytes)
        // 14-17: serial number (4 bytes)
        // 18-21: page sequence number (4 bytes) ← これをパッチ
        // 22-25: CRC (4 bytes) ← 再計算が必要
        // 26: number of segments
        // 27+: segment table

        // セグメント数を読み取ってページサイズを計算
        let num_segments = data[pos + 26] as usize;
        let header_size = 27 + num_segments;

        if pos + header_size > data.len() {
            break;
        }

        // セグメントテーブルからペイロードサイズを計算
        let payload_size: usize = data[pos + 27..pos + header_size]
            .iter()
            .map(|&x| x as usize)
            .sum();
        let page_size = header_size + payload_size;

        if pos + page_size > data.len() {
            break;
        }

        // 現在のページシーケンス番号を読み取り
        let current_seq = u32::from_le_bytes([
            data[pos + 18],
            data[pos + 19],
            data[pos + 20],
            data[pos + 21],
        ]);

        // 新しいシーケンス番号を書き込み
        let new_seq = base_sequence + current_seq;
        data[pos + 18..pos + 22].copy_from_slice(&new_seq.to_le_bytes());

        // CRCを再計算（CRCフィールドを0にしてから計算）
        data[pos + 22..pos + 26].copy_from_slice(&[0, 0, 0, 0]);
        let crc = ogg_crc32(&data[pos..pos + page_size]);
        data[pos + 22..pos + 26].copy_from_slice(&crc.to_le_bytes());

        page_count += 1;
        pos += page_size;
    }

    page_count
}

/// OGG用CRC32を計算（RFC 3533準拠）
///
/// OGGはCRC-32の変形を使用：
/// - 多項式: 0x04C11DB7
/// - 初期値: 0
/// - 反転なし
/// - XOR出力なし
fn ogg_crc32(data: &[u8]) -> u32 {
    // OGG CRC32ルックアップテーブル（事前計算済み）
    const CRC_TABLE: [u32; 256] = [
        0x00000000, 0x04c11db7, 0x09823b6e, 0x0d4326d9, 0x130476dc, 0x17c56b6b, 0x1a864db2, 0x1e475005,
        0x2608edb8, 0x22c9f00f, 0x2f8ad6d6, 0x2b4bcb61, 0x350c9b64, 0x31cd86d3, 0x3c8ea00a, 0x384fbdbd,
        0x4c11db70, 0x48d0c6c7, 0x4593e01e, 0x4152fda9, 0x5f15adac, 0x5bd4b01b, 0x569796c2, 0x52568b75,
        0x6a1936c8, 0x6ed82b7f, 0x639b0da6, 0x675a1011, 0x791d4014, 0x7ddc5da3, 0x709f7b7a, 0x745e66cd,
        0x9823b6e0, 0x9ce2ab57, 0x91a18d8e, 0x95609039, 0x8b27c03c, 0x8fe6dd8b, 0x82a5fb52, 0x8664e6e5,
        0xbe2b5b58, 0xbaea46ef, 0xb7a96036, 0xb3687d81, 0xad2f2d84, 0xa9ee3033, 0xa4ad16ea, 0xa06c0b5d,
        0xd4326d90, 0xd0f37027, 0xddb056fe, 0xd9714b49, 0xc7361b4c, 0xc3f706fb, 0xceb42022, 0xca753d95,
        0xf23a8028, 0xf6fb9d9f, 0xfbb8bb46, 0xff79a6f1, 0xe13ef6f4, 0xe5ffeb43, 0xe8bccd9a, 0xec7dd02d,
        0x34867077, 0x30476dc0, 0x3d044b19, 0x39c556ae, 0x278206ab, 0x23431b1c, 0x2e003dc5, 0x2ac12072,
        0x128e9dcf, 0x164f8078, 0x1b0ca6a1, 0x1fcdbb16, 0x018aeb13, 0x054bf6a4, 0x0808d07d, 0x0cc9cdca,
        0x7897ab07, 0x7c56b6b0, 0x71159069, 0x75d48dde, 0x6b93dddb, 0x6f52c06c, 0x6211e6b5, 0x66d0fb02,
        0x5e9f46bf, 0x5a5e5b08, 0x571d7dd1, 0x53dc6066, 0x4d9b3063, 0x495a2dd4, 0x44190b0d, 0x40d816ba,
        0xaca5c697, 0xa864db20, 0xa527fdf9, 0xa1e6e04e, 0xbfa1b04b, 0xbb60adfc, 0xb6238b25, 0xb2e29692,
        0x8aad2b2f, 0x8e6c3698, 0x832f1041, 0x87ee0df6, 0x99a95df3, 0x9d684044, 0x902b669d, 0x94ea7b2a,
        0xe0b41de7, 0xe4750050, 0xe9362689, 0xedf73b3e, 0xf3b06b3b, 0xf771768c, 0xfa325055, 0xfef34de2,
        0xc6bcf05f, 0xc27dede8, 0xcf3ecb31, 0xcbffd686, 0xd5b88683, 0xd1799b34, 0xdc3abded, 0xd8fba05a,
        0x690ce0ee, 0x6dcdfd59, 0x608edb80, 0x644fc637, 0x7a089632, 0x7ec98b85, 0x738aad5c, 0x774bb0eb,
        0x4f040d56, 0x4bc510e1, 0x46863638, 0x42472b8f, 0x5c007b8a, 0x58c1663d, 0x558240e4, 0x51435d53,
        0x251d3b9e, 0x21dc2629, 0x2c9f00f0, 0x285e1d47, 0x36194d42, 0x32d850f5, 0x3f9b762c, 0x3b5a6b9b,
        0x0315d626, 0x07d4cb91, 0x0a97ed48, 0x0e56f0ff, 0x1011a0fa, 0x14d0bd4d, 0x19939b94, 0x1d528623,
        0xf12f560e, 0xf5ee4bb9, 0xf8ad6d60, 0xfc6c70d7, 0xe22b20d2, 0xe6ea3d65, 0xeba91bbc, 0xef68060b,
        0xd727bbb6, 0xd3e6a601, 0xdea580d8, 0xda649d6f, 0xc423cd6a, 0xc0e2d0dd, 0xcda1f604, 0xc960ebb3,
        0xbd3e8d7e, 0xb9ff90c9, 0xb4bcb610, 0xb07daba7, 0xae3afba2, 0xaafbe615, 0xa7b8c0cc, 0xa379dd7b,
        0x9b3660c6, 0x9ff77d71, 0x92b45ba8, 0x9675461f, 0x8832161a, 0x8cf30bad, 0x81b02d74, 0x857130c3,
        0x5d8a9099, 0x594b8d2e, 0x5408abf7, 0x50c9b640, 0x4e8ee645, 0x4a4ffbf2, 0x470cdd2b, 0x43cdc09c,
        0x7b827d21, 0x7f436096, 0x7200464f, 0x76c15bf8, 0x68860bfd, 0x6c47164a, 0x61043093, 0x65c52d24,
        0x119b4be9, 0x155a565e, 0x18197087, 0x1cd86d30, 0x029f3d35, 0x065e2082, 0x0b1d065b, 0x0fdc1bec,
        0x3793a651, 0x3352bbe6, 0x3e119d3f, 0x3ad08088, 0x2497d08d, 0x2056cd3a, 0x2d15ebe3, 0x29d4f654,
        0xc5a92679, 0xc1683bce, 0xcc2b1d17, 0xc8ea00a0, 0xd6ad50a5, 0xd26c4d12, 0xdf2f6bcb, 0xdbee767c,
        0xe3a1cbc1, 0xe760d676, 0xea23f0af, 0xeee2ed18, 0xf0a5bd1d, 0xf464a0aa, 0xf9278673, 0xfde69bc4,
        0x89b8fd09, 0x8d79e0be, 0x803ac667, 0x84fbdbd0, 0x9abc8bd5, 0x9e7d9662, 0x933eb0bb, 0x97ffad0c,
        0xafb010b1, 0xab710d06, 0xa6322bdf, 0xa2f33668, 0xbcb4666d, 0xb8757bda, 0xb5365d03, 0xb1f740b4,
    ];

    let mut crc = 0u32;
    for &byte in data {
        let index = ((crc >> 24) ^ (byte as u32)) as usize;
        crc = (crc << 8) ^ CRC_TABLE[index];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opus_encoder_creation() {
        let encoder = OpusEncoder::new(48000, 64000).unwrap();
        assert_eq!(encoder.sample_rate(), 48000);
        assert_eq!(encoder.bitrate(), 64000);
    }

    #[test]
    fn test_encode_empty() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();
        let result = encoder.encode(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_encode_sine_wave() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();

        // 1秒間のサイン波を生成
        let samples: Vec<i16> = (0..48000)
            .map(|i| {
                let t = i as f32 / 48000.0;
                let freq = 440.0; // A4音
                ((t * freq * 2.0 * std::f32::consts::PI).sin() * 10000.0) as i16
            })
            .collect();

        let opus_data = encoder.encode(&samples).unwrap();

        // エンコードされたデータが存在する
        assert!(!opus_data.is_empty());

        // OGG-Opusヘッダーを確認
        assert!(opus_data.starts_with(b"OggS"), "OGGマジックナンバーがありません");

        // 元のPCMデータより大幅に小さい（高い圧縮効果）
        let original_size = samples.len() * 2;
        println!(
            "Opus圧縮率: {:.1}% ({} → {} bytes)",
            (opus_data.len() as f64 / original_size as f64) * 100.0,
            original_size,
            opus_data.len()
        );

        // Opusは通常90%以上の圧縮率を達成
        assert!(opus_data.len() < original_size / 5);
    }

    #[test]
    fn test_encode_silence() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();

        // 無音（全て0）
        let samples = vec![0i16; 48000];
        let opus_data = encoder.encode(&samples).unwrap();

        assert!(!opus_data.is_empty());

        println!(
            "無音Opus圧縮: {} bytes → {} bytes",
            samples.len() * 2,
            opus_data.len()
        );
    }

    #[test]
    fn test_streaming_encode() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();

        // 複数回に分けてエンコード（ストリーミングをシミュレート）
        let chunk_size = 4800; // 0.1秒分
        let mut total_output = Vec::new();

        for i in 0..10 {
            let samples: Vec<i16> = (0..chunk_size)
                .map(|j| {
                    let t = (i * chunk_size + j) as f32 / 48000.0;
                    let freq = 440.0;
                    ((t * freq * 2.0 * std::f32::consts::PI).sin() * 10000.0) as i16
                })
                .collect();

            let opus_data = encoder.encode(&samples).unwrap();
            total_output.extend_from_slice(&opus_data);
        }

        // 最初のチャンクにはOGGヘッダーが含まれる
        assert!(total_output.starts_with(b"OggS"));

        println!(
            "ストリーミングエンコード: 10チャンク → {} bytes",
            total_output.len()
        );
    }

    #[test]
    fn test_different_sample_rates() {
        for sample_rate in [16000, 24000, 48000] {
            let mut encoder = OpusEncoder::new(sample_rate, 64000).unwrap();
            let samples: Vec<i16> = (0..sample_rate as usize)
                .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
                .collect();

            let opus_data = encoder.encode(&samples).unwrap();
            assert!(!opus_data.is_empty());

            println!(
                "Sample rate {}Hz: {} bytes → {} bytes",
                sample_rate,
                samples.len() * 2,
                opus_data.len()
            );
        }
    }

    #[test]
    fn test_opus_head_format() {
        let head = create_opus_head(48000);

        // Magic signature
        assert_eq!(&head[0..8], b"OpusHead");

        // Version
        assert_eq!(head[8], 1);

        // Channel count (mono)
        assert_eq!(head[9], 1);

        // Pre-skip (little-endian)
        let pre_skip = u16::from_le_bytes([head[10], head[11]]);
        assert_eq!(pre_skip, 312);

        // Sample rate (little-endian)
        let sample_rate = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
        assert_eq!(sample_rate, 48000);
    }

    #[test]
    fn test_opus_tags_format() {
        let tags = create_opus_tags();

        // Magic signature
        assert_eq!(&tags[0..8], b"OpusTags");

        // Vendor string length
        let vendor_len = u32::from_le_bytes([tags[8], tags[9], tags[10], tags[11]]) as usize;
        assert_eq!(vendor_len, b"dcr-transcribe".len());
    }

    #[test]
    fn test_reset() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();

        // 最初のエンコード
        let samples = vec![0i16; 4800];
        let first_output = encoder.encode(&samples).unwrap();
        assert!(first_output.starts_with(b"OggS"));

        // リセット
        encoder.reset().unwrap();

        // リセット後のエンコード（再びヘッダーが含まれる）
        let second_output = encoder.encode(&samples).unwrap();
        assert!(second_output.starts_with(b"OggS"));
    }

    #[test]
    fn test_page_sequence_continuity() {
        let mut encoder = OpusEncoder::new(48000, 64000).unwrap();

        // 複数回エンコードして連結されたデータを作成
        let chunk_size = 960; // 1フレーム分（20ms）
        let mut combined_output = Vec::new();

        for _ in 0..5 {
            let samples = vec![0i16; chunk_size];
            let opus_data = encoder.encode(&samples).unwrap();
            combined_output.extend_from_slice(&opus_data);
        }

        // 連結データ内のページシーケンス番号を検証
        let mut page_sequences: Vec<u32> = Vec::new();
        let mut pos = 0;

        while pos + 27 <= combined_output.len() {
            if &combined_output[pos..pos + 4] == b"OggS" {
                // ページシーケンス番号を読み取り（bytes 18-21）
                let seq = u32::from_le_bytes([
                    combined_output[pos + 18],
                    combined_output[pos + 19],
                    combined_output[pos + 20],
                    combined_output[pos + 21],
                ]);
                page_sequences.push(seq);

                // 次のページを探す
                let num_segments = combined_output[pos + 26] as usize;
                let header_size = 27 + num_segments;
                if pos + header_size > combined_output.len() {
                    break;
                }
                let payload_size: usize = combined_output[pos + 27..pos + header_size]
                    .iter()
                    .map(|&x| x as usize)
                    .sum();
                pos += header_size + payload_size;
            } else {
                pos += 1;
            }
        }

        // ページシーケンスが0から連続して増加していることを確認
        println!("Page sequences: {:?}", page_sequences);
        assert!(!page_sequences.is_empty(), "ページが見つかりませんでした");

        for (i, &seq) in page_sequences.iter().enumerate() {
            assert_eq!(
                seq, i as u32,
                "ページシーケンスが連続していません: expected {}, got {}",
                i, seq
            );
        }

        println!(
            "ページシーケンス検証: {} ページ、すべて連続",
            page_sequences.len()
        );
    }
}

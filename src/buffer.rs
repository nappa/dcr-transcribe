use crate::config::BufferConfig;
use crate::types::{BufferedChunk, DropPolicy, SampleI16};
use std::collections::VecDeque;

/// リトライ用の音声データバッファ
///
/// ネットワーク断や API タイムアウト時のリトライに備えて
/// 音声データを一定期間保持する
pub struct AudioBuffer {
    capacity_samples: usize,
    drop_policy: DropPolicy,
    chunks: VecDeque<BufferedChunk>,
    total_samples: usize,
    sample_rate: u32,
}

impl AudioBuffer {
    pub fn new(config: &BufferConfig, sample_rate: u32) -> Self {
        let capacity_samples = (config.capacity_seconds * sample_rate) as usize;
        Self {
            capacity_samples,
            drop_policy: config.drop_policy,
            chunks: VecDeque::new(),
            total_samples: 0,
            sample_rate,
        }
    }

    /// チャンクを追加
    pub fn push(&mut self, chunk: BufferedChunk) {
        let chunk_len = chunk.samples.len();
        self.total_samples += chunk_len;
        self.chunks.push_back(chunk);

        // 容量オーバーの場合、ドロップポリシーに従って処理
        while self.total_samples > self.capacity_samples {
            match self.drop_policy {
                DropPolicy::DropOldest => {
                    if let Some(dropped) = self.chunks.pop_front() {
                        self.total_samples -= dropped.samples.len();
                    }
                }
                DropPolicy::DropNewest => {
                    if let Some(dropped) = self.chunks.pop_back() {
                        self.total_samples -= dropped.samples.len();
                    }
                }
                DropPolicy::Block => {
                    // Block ポリシーは実装しない（アーキテクチャで「使わない」と記載）
                    log::warn!("Block ポリシーは未実装: DropOldest として処理");
                    if let Some(dropped) = self.chunks.pop_front() {
                        self.total_samples -= dropped.samples.len();
                    }
                }
            }
        }
    }

    /// 指定期間のサンプルを取得
    ///
    /// # Arguments
    /// * `from_ns` - 開始タイムスタンプ (ナノ秒)
    /// * `to_ns` - 終了タイムスタンプ (ナノ秒)
    ///
    /// # Returns
    /// 指定期間内のサンプル配列
    pub fn get_range(&self, from_ns: u128, to_ns: u128) -> Vec<SampleI16> {
        let mut result = Vec::new();

        for chunk in &self.chunks {
            // チャンクの終了タイムスタンプを計算
            let chunk_duration_ns =
                (chunk.samples.len() as f64 / self.sample_rate as f64 * 1_000_000_000.0) as u128;
            let chunk_end_ns = chunk.timestamp_ns + chunk_duration_ns;

            // 範囲と重なるチャンクのみ処理
            if chunk_end_ns >= from_ns && chunk.timestamp_ns <= to_ns {
                // チャンク内の開始・終了インデックスを計算
                let start_offset = if chunk.timestamp_ns < from_ns {
                    let offset_ns = from_ns - chunk.timestamp_ns;
                    ((offset_ns as f64 / 1_000_000_000.0) * self.sample_rate as f64) as usize
                } else {
                    0
                };

                let end_offset = if chunk_end_ns > to_ns {
                    let offset_ns = to_ns - chunk.timestamp_ns;
                    ((offset_ns as f64 / 1_000_000_000.0) * self.sample_rate as f64) as usize
                } else {
                    chunk.samples.len()
                };

                if start_offset < chunk.samples.len() {
                    let end = end_offset.min(chunk.samples.len());
                    result.extend_from_slice(&chunk.samples[start_offset..end]);
                }
            }
        }

        result
    }

    /// 指定タイムスタンプより前のデータを削除
    ///
    /// # Arguments
    /// * `timestamp_ns` - このタイムスタンプより前のデータを削除
    pub fn clear_before(&mut self, timestamp_ns: u128) {
        while let Some(chunk) = self.chunks.front() {
            let chunk_duration_ns =
                (chunk.samples.len() as f64 / self.sample_rate as f64 * 1_000_000_000.0) as u128;
            let chunk_end_ns = chunk.timestamp_ns + chunk_duration_ns;

            if chunk_end_ns < timestamp_ns {
                if let Some(removed) = self.chunks.pop_front() {
                    self.total_samples -= removed.samples.len();
                }
            } else {
                break;
            }
        }
    }

    /// 最新のN秒分のデータを取得
    pub fn get_latest(&self, duration_seconds: f64) -> Vec<SampleI16> {
        let samples_needed = (duration_seconds * self.sample_rate as f64) as usize;
        let mut result = Vec::new();

        // 後ろから取得
        for chunk in self.chunks.iter().rev() {
            if result.len() >= samples_needed {
                break;
            }

            let needed = samples_needed - result.len();
            if chunk.samples.len() <= needed {
                // チャンク全体を追加（逆順なので前に追加）
                let mut temp = chunk.samples.clone();
                temp.extend(result);
                result = temp;
            } else {
                // チャンクの後ろ部分のみ追加
                let start = chunk.samples.len() - needed;
                let mut temp = chunk.samples[start..].to_vec();
                temp.extend(result);
                result = temp;
                break;
            }
        }

        result
    }

    /// バッファ内のサンプル数
    pub fn len(&self) -> usize {
        self.total_samples
    }

    /// バッファが空かどうか
    pub fn is_empty(&self) -> bool {
        self.total_samples == 0
    }

    /// バッファ内のデータ時間（秒）
    pub fn duration_seconds(&self) -> f64 {
        self.total_samples as f64 / self.sample_rate as f64
    }

    /// バッファをクリア
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.total_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_capacity() {
        let config = BufferConfig {
            capacity_seconds: 1,
            drop_policy: DropPolicy::DropOldest,
        };
        let mut buffer = AudioBuffer::new(&config, 16000);

        // 0.5秒分のデータを追加
        let chunk1 = BufferedChunk {
            samples: vec![1i16; 8000],
            timestamp_ns: 0,
        };
        buffer.push(chunk1);
        assert_eq!(buffer.len(), 8000);

        // さらに0.5秒分追加
        let chunk2 = BufferedChunk {
            samples: vec![2i16; 8000],
            timestamp_ns: 500_000_000,
        };
        buffer.push(chunk2);
        assert_eq!(buffer.len(), 16000);

        // 容量オーバー: 最古が削除される
        let chunk3 = BufferedChunk {
            samples: vec![3i16; 8000],
            timestamp_ns: 1_000_000_000,
        };
        buffer.push(chunk3);
        assert!(buffer.len() <= 16000);
    }

    #[test]
    fn test_get_latest() {
        let config = BufferConfig {
            capacity_seconds: 10,
            drop_policy: DropPolicy::DropOldest,
        };
        let mut buffer = AudioBuffer::new(&config, 16000);

        // 3チャンク追加
        buffer.push(BufferedChunk {
            samples: vec![1i16; 16000],
            timestamp_ns: 0,
        });
        buffer.push(BufferedChunk {
            samples: vec![2i16; 16000],
            timestamp_ns: 1_000_000_000,
        });
        buffer.push(BufferedChunk {
            samples: vec![3i16; 16000],
            timestamp_ns: 2_000_000_000,
        });

        // 最新1秒分を取得
        let latest = buffer.get_latest(1.0);
        assert_eq!(latest.len(), 16000);
        assert_eq!(latest[0], 3i16); // 最新チャンクのデータ
    }

    #[test]
    fn test_clear_before() {
        let config = BufferConfig {
            capacity_seconds: 10,
            drop_policy: DropPolicy::DropOldest,
        };
        let mut buffer = AudioBuffer::new(&config, 16000);

        buffer.push(BufferedChunk {
            samples: vec![1i16; 16000],
            timestamp_ns: 0,
        });
        buffer.push(BufferedChunk {
            samples: vec![2i16; 16000],
            timestamp_ns: 1_000_000_000,
        });
        buffer.push(BufferedChunk {
            samples: vec![3i16; 16000],
            timestamp_ns: 2_000_000_000,
        });

        // 1.5秒より前を削除
        buffer.clear_before(1_500_000_000);

        // 最初のチャンクは削除されているはず
        assert!(buffer.len() < 48000);
    }
}

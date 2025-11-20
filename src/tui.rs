use crate::tui_state::{ChannelState, TranscribeStatus, TuiState};
use crate::types::VadState;
use anyhow::Result;
use chrono::Timelike;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

/// TUIアプリケーション
pub struct TuiApp {
    tui_state: TuiState,
    running: Arc<AtomicBool>,
}

impl TuiApp {
    pub fn new(tui_state: TuiState, running: Arc<AtomicBool>) -> Self {
        Self { tui_state, running }
    }

    /// TUIを起動
    pub async fn run(&mut self) -> Result<()> {
        // ターミナルを初期化
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // メインループ
        loop {
            // 画面を描画
            terminal.draw(|f| self.draw(f))?;

            // イベントをポーリング（200msごと）
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            // 終了シグナルを設定
                            self.running.store(false, Ordering::SeqCst);
                            break;
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Ctrl+C で終了
                            self.running.store(false, Ordering::SeqCst);
                            break;
                        }
                        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Ctrl+Z でプロセスを一時停止
                            // まずターミナルをリストア
                            disable_raw_mode()?;
                            execute!(io::stdout(), LeaveAlternateScreen)?;

                            // プロセスを一時停止
                            #[cfg(unix)]
                            {
                                use nix::sys::signal::{self, Signal};
                                let _ = signal::raise(Signal::SIGTSTP);
                            }

                            // 再開後にターミナルを再初期化
                            enable_raw_mode()?;
                            execute!(io::stdout(), EnterAlternateScreen)?;
                        }
                        _ => {}
                    }
                }
            }

            // running フラグをチェック
            if !self.running.load(Ordering::SeqCst) {
                break;
            }
        }

        // ターミナルをリストア
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        Ok(())
    }

    /// 画面を描画
    fn draw(&self, f: &mut Frame) {
        let channels = self.tui_state.get_all_channels();

        if channels.is_empty() {
            let block = Block::default()
                .title("dcr-transcribe")
                .borders(Borders::ALL);
            let paragraph = Paragraph::new("チャンネルがありません").block(block);
            f.render_widget(paragraph, f.area());
            return;
        }

        // チャンネル数に応じて横方向に分割
        let constraints: Vec<Constraint> = channels
            .iter()
            .map(|_| Constraint::Percentage((100 / channels.len()) as u16))
            .collect();

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(f.area());

        // 各チャンネルを描画
        for (i, channel) in channels.iter().enumerate() {
            if i < chunks.len() {
                self.draw_channel(f, chunks[i], channel);
            }
        }
    }

    /// 1つのチャンネルを描画
    fn draw_channel(&self, f: &mut Frame, area: Rect, channel: &ChannelState) {
        // チャンネル全体のブロック
        let block = Block::default()
            .title(format!(
                "Channel {} - {}",
                channel.channel_id, channel.channel_name
            ))
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White));

        let inner_area = block.inner(area);
        f.render_widget(block, area);

        // 内部を3つの領域に分割
        // 1. ボリューム表示（1行）
        // 2. ステータス表示（1行）
        // 3. Transcribe結果表示（残り）
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // ボリュームバー
                Constraint::Length(1), // ステータス
                Constraint::Min(0),    // Transcribe結果
            ])
            .split(inner_area);

        // 1. ボリューム表示
        self.draw_volume_bar(f, sections[0], channel);

        // 2. ステータス表示
        self.draw_status(f, sections[1], channel);

        // 3. Transcribe結果表示
        self.draw_transcripts(f, sections[2], channel);
    }

    /// ボリュームバーを描画
    fn draw_volume_bar(&self, f: &mut Frame, area: Rect, channel: &ChannelState) {
        // リアルタイムボリューム
        let current_ratio = Self::db_to_ratio(channel.current_volume_db);

        // VAD閾値の位置を計算（0.0～1.0の範囲）
        let threshold_ratio = Self::db_to_ratio(channel.vad_threshold_db);
        let threshold_position = (threshold_ratio * area.width as f64) as u16;

        // ラベルに閾値情報を追加
        let label = format!(
            "音量: {:.1} dB (閾値: {:.1} dB)",
            channel.current_volume_db,
            channel.vad_threshold_db
        );

        let current_gauge = Gauge::default()
            .label(label)
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(current_ratio);
        f.render_widget(current_gauge, area);

        // 閾値の位置にマーカーを表示（縦線）
        if threshold_position < area.width {
            let marker_x = area.x + threshold_position;
            let marker = Paragraph::new("|")
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));

            let marker_area = Rect {
                x: marker_x,
                y: area.y,
                width: 1,
                height: 1,
            };
            f.render_widget(marker, marker_area);
        }
    }

    /// ステータス表示を描画
    fn draw_status(&self, f: &mut Frame, area: Rect, channel: &ChannelState) {
        // VAD状態と無音持続時間
        let (vad_color, vad_text) = match channel.vad_state {
            VadState::Silence => {
                if let Some(duration) = channel.silence_duration_secs() {
                    (Color::Gray, format!("無音 ({:.1}秒)", duration))
                } else {
                    (Color::Gray, "無音".to_string())
                }
            }
            VadState::Voice { .. } => (Color::Blue, "音声".to_string()),
        };

        // Transcribe接続状態
        let (transcribe_color, transcribe_text) = match channel.transcribe_status {
            TranscribeStatus::Connected => (Color::Green, "正常"),
            TranscribeStatus::Error => (Color::Red, "エラー"),
            TranscribeStatus::Disconnected => (Color::Gray, "無通信"),
        };

        let status_line = Line::from(vec![
            Span::styled("VAD: ", Style::default().fg(Color::White)),
            Span::styled(
                vad_text,
                Style::default()
                    .fg(vad_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Transcribe: ", Style::default().fg(Color::White)),
            Span::styled(
                transcribe_text,
                Style::default()
                    .fg(transcribe_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        let paragraph = Paragraph::new(status_line);
        f.render_widget(paragraph, area);
    }

    /// Transcribe結果を描画
    fn draw_transcripts(&self, f: &mut Frame, area: Rect, channel: &ChannelState) {
        let lines: Vec<Line> = channel
            .transcripts
            .iter()
            .rev() // 最新が上
            .map(|entry| {
                // ISO 8601形式のタイムスタンプから時:分を抽出
                let time_str = Self::extract_time_hhmm(&entry.time);
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", time_str),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(&entry.text),
                ])
            })
            .collect();

        let text = Text::from(lines);
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::NONE))
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    }

    /// dBを0.0～1.0の比率に変換
    /// -60dB～0dB を 0.0～1.0 にマッピング
    fn db_to_ratio(db: f32) -> f64 {
        let min_db = -60.0;
        let max_db = 0.0;
        let clamped = db.clamp(min_db, max_db);
        ((clamped - min_db) / (max_db - min_db)) as f64
    }

    /// ISO 8601形式のタイムスタンプからHH:MMフォーマットを抽出
    fn extract_time_hhmm(timestamp: &str) -> String {
        // ISO 8601形式（例: "2025-01-04T12:34:56+09:00"）から時:分を抽出
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            // ローカルタイムゾーンに変換
            let local_dt = dt.with_timezone(&chrono::Local);
            format!("{:02}:{:02}", local_dt.hour(), local_dt.minute())
        } else {
            // パース失敗時はタイムスタンプの一部を抽出する簡易版
            // "2025-01-04T12:34:56" の形式から "12:34" を抽出
            if timestamp.len() >= 16 {
                let time_part = &timestamp[11..16]; // "12:34"
                time_part.to_string()
            } else {
                "--:--".to_string()
            }
        }
    }
}

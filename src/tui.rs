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
    widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap},
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
    /// 終了確認ダイアログを表示中かどうか
    exit_confirm_shown: bool,
}

impl TuiApp {
    pub fn new(tui_state: TuiState, running: Arc<AtomicBool>) -> Self {
        Self {
            tui_state,
            running,
            exit_confirm_shown: false,
        }
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
                    // 終了確認ダイアログが表示されている場合
                    if self.exit_confirm_shown {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                // 終了を確定
                                self.running.store(false, Ordering::SeqCst);
                                break;
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                // キャンセル
                                self.exit_confirm_shown = false;
                            }
                            _ => {}
                        }
                    } else {
                        // 通常のキー入力処理
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                // 終了確認ダイアログを表示
                                self.exit_confirm_shown = true;
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                // Ctrl+C で即座に終了（確認なし）
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
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                // 数字キーでチャンネルを選択（1キー→Ch0, 2キー→Ch1, 3キー→Ch2, 4キー→Ch3）
                                if let Some(digit) = c.to_digit(10) {
                                    if digit >= 1 && digit <= 9 {
                                        let channel_id = (digit - 1) as usize;  // 1→0, 2→1, 3→2, 4→3
                                        let channels = self.tui_state.get_all_channels();

                                        // 該当するチャンネルが存在するか確認
                                        if channels.iter().any(|ch| ch.channel_id == channel_id) {
                                            // 現在の選択と同じなら選択解除、異なるなら選択
                                            let current_selection = self.tui_state.get_selected_channel_for_output();
                                            if current_selection == Some(channel_id) {
                                                self.tui_state.set_selected_channel_for_output(None);
                                            } else {
                                                self.tui_state.set_selected_channel_for_output(Some(channel_id));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
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

        // 選択されているチャンネルIDを取得
        let selected_channel_id = self.tui_state.get_selected_channel_for_output();

        // 各チャンネルを描画
        for (i, channel) in channels.iter().enumerate() {
            if i < chunks.len() {
                let is_selected = selected_channel_id == Some(channel.channel_id);
                self.draw_channel(f, chunks[i], channel, is_selected);
            }
        }

        // 終了確認ダイアログを描画
        if self.exit_confirm_shown {
            self.draw_exit_confirm_dialog(f);
        }
    }

    /// 1つのチャンネルを描画
    fn draw_channel(&self, f: &mut Frame, area: Rect, channel: &ChannelState, is_selected: bool) {
        // 選択されている場合はタイトルに [出力中] を追加し、色を変更
        let title = if is_selected {
            format!(
                "{}: {} [出力中]",
                channel.channel_id + 1, channel.channel_name
            )
        } else {
            format!(
                "{}: {}",
                channel.channel_id + 1, channel.channel_name
            )
        };

        let border_color = if is_selected {
            Color::Yellow
        } else {
            Color::White
        };

        // チャンネル全体のブロック
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .style(Style::default().fg(border_color));

        let inner_area = block.inner(area);
        f.render_widget(block, area);

        // 内部を4つの領域に分割
        // 1. Transcribe結果表示（上部、ほとんどのスペース）
        // 2. 空白行（1行）
        // 3. ボリューム表示（下部、1行）
        // 4. ステータス表示（下部、1行）
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),    // Transcribe結果
                Constraint::Length(1), // 空白行
                Constraint::Length(1), // ボリュームバー
                Constraint::Length(1), // ステータス
            ])
            .split(inner_area);

        // 1. Transcribe結果表示
        self.draw_transcripts(f, sections[0], channel);

        // 2. 空白行（何も描画しない）

        // 3. ボリューム表示
        self.draw_volume_bar(f, sections[2], channel);

        // 4. ステータス表示
        self.draw_status(f, sections[3], channel);
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

        // 音量バーの色を決定
        use crate::types::VadState;
        let gauge_color = match channel.vad_state {
            VadState::Silence => Color::Gray,  // 無音検出時は灰色
            VadState::Voice { .. } => {
                if channel.current_volume_db >= -30.0 {
                    Color::Red  // -30dB以上は赤色
                } else {
                    Color::Cyan  // それ以外はシアン
                }
            }
        };

        let current_gauge = Gauge::default()
            .label(label)
            .gauge_style(Style::default().fg(gauge_color))
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
        // VAD状態
        let (vad_color, vad_text) = match channel.vad_state {
            VadState::Silence => (Color::Gray, "無音".to_string()),
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
        let available_height = area.height as usize;
        let available_width = area.width as usize;

        // タイムスタンプのフォーマット: "[HH:MM:SS] "
        let timestamp_width = 11; // "[12:34:56] ".len()
        let first_line_text_width = available_width.saturating_sub(timestamp_width);

        // まず全結果の必要行数を計算（古い順）
        let mut entries_with_lines: Vec<Vec<Line>> = Vec::new();

        // 確定結果を古い順に処理
        for entry in channel.transcripts.iter() {
            let time_str = Self::extract_time_hhmmss(&entry.time);
            let wrapped_lines = Self::wrap_text_with_timestamp(
                &time_str,
                &entry.text,
                first_line_text_width,
                available_width,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            );

            entries_with_lines.push(wrapped_lines);
        }

        // 部分結果を最後に追加（あれば）
        if let Some(partial) = &channel.partial_transcript {
            let time_str = Self::extract_time_hhmmss(&partial.time);

            // stabilityに応じて色を変更
            let text_color = match partial.stability {
                Some(crate::types::Stability::Low) => Color::DarkGray,
                Some(crate::types::Stability::Medium) => Color::Gray,
                Some(crate::types::Stability::High) | None => Color::White,
            };

            let wrapped_lines = Self::wrap_text_with_timestamp(
                &time_str,
                &partial.text,
                first_line_text_width,
                available_width,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                Style::default().fg(text_color).add_modifier(Modifier::ITALIC),
            );
            entries_with_lines.push(wrapped_lines);
        }

        // 全ての行を結合（スキップは後で行単位で行う）
        let mut all_lines: Vec<Line> = Vec::new();
        for lines in entries_with_lines.into_iter() {
            all_lines.extend(lines);
        }

        // 表示可能な行数を超えている場合、最新の行が見えるように古い行をスキップ
        let lines_to_display = if all_lines.len() > available_height {
            // 最新のavailable_height行のみを表示（最後の部分が常に表示される）
            all_lines.split_off(all_lines.len() - available_height)
        } else {
            all_lines
        };

        let text = Text::from(lines_to_display);
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::NONE));
        f.render_widget(paragraph, area);
    }

    /// テキストを折り返してタイムスタンプ付きの行に変換
    fn wrap_text_with_timestamp(
        timestamp: &str,
        text: &str,
        first_line_text_width: usize,
        available_width: usize,
        timestamp_style: Style,
        text_style: Style,
    ) -> Vec<Line<'static>> {
        if first_line_text_width == 0 {
            return vec![];
        }

        let mut lines = Vec::new();
        let mut remaining = text;
        let mut is_first_line = true;

        while !remaining.is_empty() {
            // 1行目はタイムスタンプの幅を引いた幅、2行目以降は全幅を使う
            let line_width = if is_first_line {
                first_line_text_width
            } else {
                available_width
            };

            // Unicode文字を考慮した幅計算
            let mut char_count = 0;
            let mut byte_count = 0;
            let mut current_width = 0;

            for ch in remaining.chars() {
                let char_width = if ch.is_ascii() { 1 } else { 2 }; // 全角文字は幅2

                if current_width + char_width > line_width {
                    break;
                }

                current_width += char_width;
                byte_count += ch.len_utf8();
                char_count += 1;
            }

            // 少なくとも1文字は含める
            if char_count == 0 && !remaining.is_empty() {
                let first_char = remaining.chars().next().unwrap();
                byte_count = first_char.len_utf8();
            }

            let line_text = &remaining[..byte_count];
            remaining = &remaining[byte_count..];

            if is_first_line {
                // 最初の行：タイムスタンプを含める
                lines.push(Line::from(vec![
                    Span::styled(format!("[{}] ", timestamp), timestamp_style),
                    Span::styled(line_text.to_string(), text_style),
                ]));
                is_first_line = false;
            } else {
                // 2行目以降：インデントなし、全幅を使う
                lines.push(Line::from(vec![
                    Span::styled(line_text.to_string(), text_style),
                ]));
            }
        }

        lines
    }

    /// dBを0.0～1.0の比率に変換
    /// -60dB～0dB を 0.0～1.0 にマッピング
    fn db_to_ratio(db: f32) -> f64 {
        let min_db = -60.0;
        let max_db = 0.0;
        let clamped = db.clamp(min_db, max_db);
        ((clamped - min_db) / (max_db - min_db)) as f64
    }

    /// ISO 8601形式のタイムスタンプからHH:MM:SSフォーマットを抽出
    fn extract_time_hhmmss(timestamp: &str) -> String {
        // ISO 8601形式（例: "2025-01-04T12:34:56+09:00"）から時:分:秒を抽出
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            // ローカルタイムゾーンに変換
            let local_dt = dt.with_timezone(&chrono::Local);
            format!(
                "{:02}:{:02}:{:02}",
                local_dt.hour(),
                local_dt.minute(),
                local_dt.second()
            )
        } else {
            // パース失敗時はタイムスタンプの一部を抽出する簡易版
            // "2025-01-04T12:34:56" の形式から "12:34:56" を抽出
            if timestamp.len() >= 19 {
                let time_part = &timestamp[11..19]; // "12:34:56"
                time_part.to_string()
            } else {
                "--:--:--".to_string()
            }
        }
    }

    /// 終了確認ダイアログを描画
    fn draw_exit_confirm_dialog(&self, f: &mut Frame) {
        // 画面中央にダイアログを配置
        let area = f.area();

        // ダイアログのサイズを計算（幅50%、高さ30%）
        let dialog_width = area.width.saturating_mul(50) / 100;
        let dialog_height = 7;

        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect {
            x: dialog_x,
            y: dialog_y,
            width: dialog_width,
            height: dialog_height,
        };

        // ダイアログの背景を完全にクリア（裏の文字を消す）
        f.render_widget(Clear, dialog_area);

        // ダイアログボックス
        let block = Block::default()
            .title("確認")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .style(Style::default().bg(Color::Black).fg(Color::White));

        let inner_area = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        // メッセージを表示
        let message = vec![
            Line::from(""),
            Line::from(Span::styled(
                "本当に終了しますか？",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("Y", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(": はい  "),
                Span::styled("N", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" / "),
                Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(": いいえ"),
            ]),
        ];

        let paragraph = Paragraph::new(message)
            .style(Style::default().bg(Color::Black))
            .alignment(ratatui::layout::Alignment::Center);

        f.render_widget(paragraph, inner_area);
    }
}

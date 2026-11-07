use crate::share::{qr_modules, ShareInfo};
use crate::tui_state::{ChannelState, Health, TranscribeStatus, TuiState};
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
    widgets::{Block, Borders, Clear, Gauge, Paragraph},
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
    /// 閲覧画面のログイン URL(Remote サーバが無効なら None)
    share: Option<ShareInfo>,
    /// ログイン URL / QR の窓を表示中か
    share_shown: bool,
    /// 窓で公開 URL ではなくローカル URL を出すか
    share_local: bool,
    flash_clock: std::time::Instant,
}

impl TuiApp {
    pub fn new(tui_state: TuiState, running: Arc<AtomicBool>) -> Self {
        Self {
            tui_state,
            running,
            exit_confirm_shown: false,
            share: None,
            share_shown: false,
            share_local: false,
            flash_clock: std::time::Instant::now(),
        }
    }

    /// ログイン URL / QR 表示を有効にする
    pub fn with_share(mut self, share: Option<ShareInfo>) -> Self {
        self.share = share;
        self
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
            // 中継サーバのログイン URL はつながってから届くので、共有状態から取り直す
            if let Some(share) = self.tui_state.share_info.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                self.share = Some(share);
            }
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
                    } else if self.share_shown {
                        // ログイン URL / QR の窓: u / Esc で閉じる、l で公開 / ローカルを切り替え
                        match key.code {
                            KeyCode::Char('u') | KeyCode::Char('U') | KeyCode::Esc => self.share_shown = false,
                            KeyCode::Char('l') | KeyCode::Char('L') => self.share_local = !self.share_local,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.running.store(false, Ordering::SeqCst);
                                break;
                            }
                            _ => {}
                        }
                    } else {
                        // 通常のキー入力処理
                        match key.code {
                            KeyCode::Char('u') | KeyCode::Char('U') if self.share.is_some() => {
                                // ログインキーを含むので、開いたときだけ表示する
                                self.share_shown = true;
                                self.share_local = self.share.as_ref().is_some_and(|s| s.public.is_none());
                                // ローカル URL が無ければ公開 URL だけ
                                if self.share.as_ref().is_some_and(|s| s.local.is_none()) {
                                    self.share_local = false;
                                }
                            }
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
                                            self.tui_state.toggle_output(channel_id);
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
            self.draw_device_alert(f, self.flash_clock.elapsed().as_secs() % 2 == 0);
            return;
        }

        // 最下行はキー操作の案内
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());
        self.draw_help(f, rows[1]);

        // チャンネル数に応じて横方向に分割
        let constraints: Vec<Constraint> = channels
            .iter()
            .map(|_| Constraint::Percentage((100 / channels.len()) as u16))
            .collect();

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(rows[0]);

        // 選択されているチャンネルIDを取得
        let selected_channels = self.tui_state.get_selected_channels_for_output();

        // 各チャンネルを描画
        for (i, channel) in channels.iter().enumerate() {
            if i < chunks.len() {
                let is_selected = selected_channels.contains(&channel.channel_id);
                self.draw_channel(f, chunks[i], channel, is_selected);
            }
        }

        if self.share_shown {
            self.draw_share(f);
        }

        self.draw_device_alert(f, self.flash_clock.elapsed().as_secs() % 2 == 0);

        // 終了確認ダイアログを描画
        if self.exit_confirm_shown {
            self.draw_exit_confirm_dialog(f);
        }
    }

    fn draw_device_alert(&self, f: &mut Frame, flash: bool) {
        let mut messages: Vec<String> = [
            ("入力", &self.tui_state.input_health),
            ("出力", &self.tui_state.output_health),
        ].into_iter().filter_map(|(label, health)| {
            health.problem().map(|problem| format!("{label}: {problem}"))
        }).collect();
        messages.extend(self.tui_state.get_all_channels().into_iter().filter_map(|channel| {
            channel.recording_error.map(|error| format!("{}: {error}", channel.channel_name))
        }));
        if messages.is_empty() { return; }
        let area = if flash { f.area() } else {
            Rect { height: f.area().height.min(4), ..f.area() }
        };
        f.render_widget(Clear, area);
        let message = format!("{}\n接続・保存先を確認してください。復帰しない場合は再起動。 q: 終了", messages.join("\n"));
        f.render_widget(Paragraph::new(message)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .style(Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD))
            .block(Block::default().title("音声・録音警告").borders(Borders::ALL)), area);
    }

    /// 最下行のキー操作の案内
    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        let mut spans = Vec::new();
        if self.share.is_some() {
            spans.extend([key(" u"), Span::raw(": ログイン URL / QR  ")]);
        }
        spans.extend([key(" 1-9"), Span::raw(": 音声出力  "), key(" q"), Span::raw(": 終了")]);
        if let Some((health, up)) = self.tui_state.uplink_status.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            spans.extend([Span::raw("   "), Span::styled(format!("● {}", up), Style::default().fg(health_color(health)))]);
        }
        // この機械の時計のずれ(測れるまでは灰)
        let clock = &self.tui_state.clock;
        let (health, text) = match clock.offset_ms() {
            Some(o) if o.abs() > clock.warn_ms() => (Health::Warn, format!("時計 {:+.1}s", o as f64 / 1000.0)),
            Some(o) => (Health::Good, format!("時計 {:+}ms", o)),
            None => (Health::Unknown, "時計 未測定".to_string()),
        };
        spans.extend([Span::raw("   "), Span::styled(format!("● {}", text), Style::default().fg(health_color(health)))]);
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::Gray)), area);
    }

    /// ログイン URL と QR コードの窓
    fn draw_share(&self, f: &mut Frame) {
        let Some(share) = &self.share else { return };
        let (label, url) = match (&share.public, &share.local, self.share_local) {
            (Some(public), _, false) | (Some(public), None, _) => ("公開 URL", public.as_str()),
            (_, Some(local), _) => ("ローカル URL(このパソコンからのみ)", local.as_str()),
            (None, None, _) => return,
        };
        let qr = qr_modules(url).ok();
        // QR は上下 2 モジュールを 1 文字(▀)で描くので、高さは半分
        let qr_w = qr.as_ref().map_or(0, |m| m.len()) as u16;
        let qr_h = qr.as_ref().map_or(0, |m| m.len().div_ceil(2)) as u16;

        let area = f.area();
        let url_w = unicode_width(url) as u16;
        let width = (qr_w.max(url_w).max(44) + 4).min(area.width);
        let height = (qr_h + 7).min(area.height);
        let popup = Rect {
            x: area.width.saturating_sub(width) / 2,
            y: area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, popup);
        let switch_hint = if share.public.is_some() && share.local.is_some() { "  l: 公開/ローカル切替" } else { "" };
        let block = Block::default()
            .title(format!(" {} ", label))
            .title_bottom(format!(" u / Esc: 閉じる{} ", switch_hint))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black).fg(Color::White));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        let mut lines = vec![Line::from(Span::styled(url, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))];
        lines.push(Line::from(""));
        match &qr {
            Some(m) if inner.width >= qr_w && inner.height >= qr_h + 2 => {
                // 端末の配色に関係なく読めるよう、白地に黒で描く
                let color = |dark: bool| if dark { Color::Black } else { Color::White };
                for y in (0..m.len()).step_by(2) {
                    let spans: Vec<Span> = (0..m.len())
                        .map(|x| {
                            let top = m[y][x];
                            let bottom = m.get(y + 1).is_some_and(|r| r[x]);
                            Span::styled("▀", Style::default().fg(color(top)).bg(color(bottom)))
                        })
                        .collect();
                    lines.push(Line::from(spans));
                }
            }
            Some(_) => {
                lines.push(Line::from("端末が小さくて QR を出せません"));
                lines.push(Line::from("端末を広げてください"));
            }
            None => lines.push(Line::from("QR コードを作れませんでした")),
        }
        // 長い URL は折り返す(QR の行は幅に収まっているので折り返されない)
        f.render_widget(
            Paragraph::new(lines)
                .alignment(ratatui::layout::Alignment::Center)
                .wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
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

        let title = if channel.input_quality_warning.is_some() {
            format!("{title} [入力品質警告]")
        } else { title };

        let border_color = if channel.input_quality_warning.is_some() {
            Color::Yellow
        } else if is_selected {
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
                Constraint::Length(if channel.input_quality_warning.is_some() { 2 } else { 1 }), // 入力品質警告
                Constraint::Length(1), // ボリュームバー
                Constraint::Length(1), // ステータス
            ])
            .split(inner_area);

        // 1. Transcribe結果表示
        self.draw_transcripts(f, sections[0], channel);

        // 2. 品質警告は録音停止の赤い警告と区別する。
        if let Some(warning) = &channel.input_quality_warning {
            f.render_widget(Paragraph::new(warning.as_str())
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().fg(Color::Yellow)), sections[1]);
        }

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
            timestamp.get(..19)
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
                .map(|dt| dt.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "--:--:--".to_string())
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

/// 端末での表示幅(URL は ASCII なので文字数で足りる)
fn unicode_width(s: &str) -> usize {
    s.chars().count()
}

/// 状態表示の色(緑・黄・赤・灰)
fn health_color(h: Health) -> Color {
    match h {
        Health::Good => Color::Green,
        Health::Warn => Color::Yellow,
        Health::Bad => Color::Red,
        Health::Unknown => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn app(share: Option<ShareInfo>) -> TuiApp {
        let state = TuiState::new();
        state.add_channel(0, "L".into());
        TuiApp::new(state, Arc::new(AtomicBool::new(true))).with_share(share)
    }

    fn render(app: &TuiApp, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        term.backend().buffer().clone()
    }

    /// 画面の文字を連結する。全角文字は 2 マス使い、次のマスは空なので読み飛ばす
    fn text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        let mut skip = false;
        for c in buf.content() {
            if std::mem::take(&mut skip) {
                continue;
            }
            out.push_str(c.symbol());
            skip = !c.symbol().is_ascii() && c.symbol() != "▀";
        }
        out
    }

    fn share() -> ShareInfo {
        ShareInfo {
            local: Some("http://127.0.0.1:8765/#k=abc".into()),
            public: Some("https://t.example.com/#k=abc".into()),
        }
    }

    #[test]
    fn help_bar_mentions_url_key_only_when_available() {
        assert!(text(&render(&app(Some(share())), 100, 30)).contains("ログイン URL / QR"));
        assert!(!text(&render(&app(None), 100, 30)).contains("ログイン URL / QR"));
    }

    #[test]
    fn share_popup_is_hidden_until_opened() {
        let a = app(Some(share()));
        assert!(!text(&render(&a, 100, 40)).contains("https://t.example.com/#k=abc"));
    }

    #[test]
    fn share_popup_draws_url_and_qr_matching_modules() {
        let mut a = app(Some(share()));
        a.share_shown = true;
        let buf = render(&a, 100, 40);
        let all = text(&buf);
        assert!(all.contains("https://t.example.com/#k=abc"));
        assert!(all.contains("公開 URL"));

        // ▀ の前景 = 上のモジュール、背景 = 下のモジュール(黒 = 暗)。QR の左上隅を探して照合する
        let m = qr_modules("https://t.example.com/#k=abc").unwrap();
        let area = buf.area;
        let origin = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buf[(x, y)].symbol() == "▀")
            .expect("QR が描かれている");
        for (row, y) in (0..m.len()).step_by(2).enumerate() {
            for x in 0..m.len() {
                let cell = &buf[(origin.0 + x as u16, origin.1 + row as u16)];
                let dark = |c: Color| c == Color::Black;
                assert_eq!(dark(cell.fg), m[y][x], "({}, {})", x, y);
                assert_eq!(dark(cell.bg), m.get(y + 1).is_some_and(|r| r[x]), "({}, {})", x, y + 1);
            }
        }

        // l でローカル URL に切り替え
        a.share_local = true;
        assert!(text(&render(&a, 100, 40)).contains("http://127.0.0.1:8765/#k=abc"));
    }

    #[test]
    fn share_popup_explains_when_terminal_too_small() {
        let mut a = app(Some(share()));
        a.share_shown = true;
        assert!(text(&render(&a, 60, 15)).contains("端末が小さくて"));
    }
    #[test]
    fn regression_malformed_timestamps_do_not_panic() {
        for value in ["", "短い", "あいうえおかきくけこ", "xxxxxxxxxxxxxxxxxxxx"] {
            assert_eq!(TuiApp::extract_time_hhmmss(value), "--:--:--");
        }
        assert_eq!(TuiApp::extract_time_hhmmss("2026-10-17T12:34:56"), "12:34:56");
    }

    #[test]
    fn device_error_flashes_screen_and_keeps_banner_between_flashes() {
        let a = app(None);
        a.tui_state.input_health.start();
        a.tui_state.input_health.report_error();
        for flash in [true, false] {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|f| a.draw_device_alert(f, flash)).unwrap();
            let buf = terminal.backend().buffer();
            assert!(text(buf).contains("入力: デバイスエラー"));
            assert_eq!(buf[(99, 29)].bg == Color::Red, flash);
        }
        a.tui_state.input_health.start();
        assert!(!text(&render(&a, 100, 30)).contains("音声・録音警告"));
    }

    #[test]
    fn alerts_survive_small_terminal_and_do_not_hide_quit_confirmation() {
        let mut a = app(None);
        a.tui_state.output_health.start();
        a.tui_state.output_health.report_error();
        for (width, height) in [(0, 0), (1, 1), (10, 2)] { render(&a, width, height); }
        a.exit_confirm_shown = true;
        assert!(text(&render(&a, 100, 30)).contains("確認"));
        a.tui_state.output_health.stop();
        a.tui_state.update_channel(0, |ch| ch.recording_error = Some("録音停止".into()));
        assert!(text(&render(&a, 100, 30)).contains("録音停止"));
    }

    #[test]
    fn input_quality_warning_is_visible_and_clears_without_full_screen_alarm() {
        let a = app(None);
        a.tui_state.update_channel(0, |ch| ch.input_quality_warning = Some("ハム成分が継続。接続を確認".into()));
        let content = text(&render(&a, 100, 30));
        assert!(content.contains("ハム成分"));
        assert!(content.contains("入力品質警告"));
        assert!(!content.contains("音声・録音警告"));
        for (w, h) in [(0, 0), (1, 1), (10, 2)] { render(&a, w, h); }
        a.tui_state.update_channel(0, |ch| ch.input_quality_warning = None);
        assert!(!text(&render(&a, 100, 30)).contains("入力品質警告"));
    }

}

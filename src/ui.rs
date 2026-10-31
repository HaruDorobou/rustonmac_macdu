use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::app::App;

const BAR_WIDTH: usize = 20;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(3), // 헤더
        Constraint::Min(0),    // 목록
        Constraint::Length(1), // 푸터
    ])
    .split(f.area());

    // 헤더: 스캔 중이면 스피너와 대상 경로, 아니면 현재 경로와 총 용량
    const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let header_text = match &app.scanning {
        Some(dir) => format!(
            " {} 스캔 중… {}",
            SPINNER[app.tick / 2 % SPINNER.len()],
            dir.display()
        ),
        None => {
            let took = app
                .last_scan
                .map(|d| format!(" · 스캔 {:.2}초", d.as_secs_f64()))
                .unwrap_or_default();
            format!(
                " {}   (합계: {}{})",
                app.current.display(),
                human(app.total_size()),
                took
            )
        }
    };
    let header = Paragraph::new(header_text)
        .block(Block::default().borders(Borders::ALL).title(" mac-du "));
    f.render_widget(header, chunks[0]);

    // 목록: 용량 막대 + 비율 + 이름
    let total = app.total_size().max(1);
    let max = app.entries.iter().map(|e| e.size).max().unwrap_or(1).max(1);

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let filled = (e.size as f64 / max as f64 * BAR_WIDTH as f64).round() as usize;
            let bar = format!("{}{}", "█".repeat(filled), " ".repeat(BAR_WIDTH - filled));
            let pct = e.size as f64 / total as f64 * 100.0;
            let icon = if e.is_dir { "📁" } else { "📄" };

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>10} ", human(e.size)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!("{:>5.1}% ", pct),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {} {}", icon, e.name)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut app.state);

    // 푸터: 오류 메시지가 있으면 그것을, 없으면 키 도움말
    let footer = match &app.status {
        Some(msg) => Paragraph::new(format!(" ⚠ {}", msg)).style(Style::default().fg(Color::Red)),
        None => Paragraph::new(" j/k: 이동   l/Enter: 들어가기   h/Backspace: 상위   q/Esc: 종료")
            .style(Style::default().fg(Color::DarkGray)),
    };
    f.render_widget(footer, chunks[2]);
}

/// 바이트를 사람이 읽기 좋은 단위로 바꾼다.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", size, UNITS[i])
    }
}

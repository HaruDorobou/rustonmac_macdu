use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
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
            // 읽지 못한 경로가 있으면 합계는 실제보다 작을 수 있다
            let at_least = if app.errors.total > 0 { "≥ " } else { "" };
            format!(
                " {}   (합계: {}{}{})",
                app.current.display(),
                at_least,
                human(app.total_size()),
                took
            )
        }
    };
    let mut header_spans = vec![Span::raw(header_text)];
    if app.scanning.is_none() && app.errors.total > 0 {
        header_spans.push(Span::styled(
            format!("   ⚠ 읽지 못한 경로 {}건 (e)", app.errors.total),
            Style::default().fg(Color::Yellow),
        ));
    }
    let header = Paragraph::new(Line::from(header_spans))
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
                // 읽지 못한 부분이 있으면 크기가 실제보다 작다는 표시
                Span::styled(
                    if e.unreadable > 0 {
                        format!("  ⚠ 읽지 못함 {}", e.unreadable)
                    } else {
                        String::new()
                    },
                    Style::default().fg(Color::Yellow),
                ),
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
        None => {
            let errors_hint = if app.errors.total > 0 { "   e: 오류 목록" } else { "" };
            Paragraph::new(format!(
                " j/k: 이동   l/Enter: 들어가기   h/Backspace: 상위   r: 다시 스캔{}   q/Esc: 종료",
                errors_hint
            ))
            .style(Style::default().fg(Color::DarkGray))
        }
    };
    f.render_widget(footer, chunks[2]);

    if app.show_errors {
        draw_errors(f, app);
    }
}

/// 읽지 못한 경로 목록을 화면 가운데 팝업으로 보여 준다.
fn draw_errors(f: &mut Frame, app: &mut App) {
    let area = centered(f.area(), 90, 70);
    f.render_widget(Clear, area);

    let shown = app.errors.list.len();
    let title = if app.errors.total > shown {
        format!(
            " 읽지 못한 경로 {}건 (앞 {}건만 표시) — j/k 이동, Esc 닫기 ",
            app.errors.total, shown
        )
    } else {
        format!(
            " 읽지 못한 경로 {}건 — j/k 이동, Esc 닫기 ",
            app.errors.total
        )
    };

    let items: Vec<ListItem> = app
        .errors
        .list
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("[{}] ", describe_errno(e.errno)),
                    Style::default().fg(Color::Red),
                ),
                Span::raw(e.path.display().to_string()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom(" 허용 안 됨: 터미널에 '전체 디스크 접근 권한'이 필요할 수 있습니다 "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.error_state);
}

fn describe_errno(errno: i32) -> String {
    match errno {
        1 => "허용 안 됨".into(),
        2 => "사라짐".into(),
        13 => "권한 없음".into(),
        0 => "알 수 없음".into(),
        n => format!("오류 {}", n),
    }
}

/// `area` 안에서 가로 `pct_x`%, 세로 `pct_y`% 크기의 가운데 영역
fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(vertical[1])[1]
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

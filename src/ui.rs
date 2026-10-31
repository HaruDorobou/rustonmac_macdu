use std::time::{Duration, Instant};

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, Change};

const BAR_WIDTH: usize = 20;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 변화가 막 감지된 것으로 보고 눈에 띄게 강조하는 시간
const FRESH: Duration = Duration::from_secs(3);
/// 헤더 테두리를 깜빡이는 시간
const FLASH: Duration = Duration::from_millis(1500);

pub fn draw(f: &mut Frame, app: &mut App) {
    let header_height = if app.watch.is_some() { 4 } else { 3 };
    let chunks = Layout::vertical([
        Constraint::Length(header_height), // 헤더
        Constraint::Min(0),                // 목록
        Constraint::Length(1),             // 푸터
    ])
    .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_list(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.show_errors {
        draw_errors(f, app);
    }
    if app.show_expand {
        draw_expand(f, app);
    }
}

/// 어느 상위 폴더까지 미리 스캔해 둘지 고르는 팝업
fn draw_expand(f: &mut Frame, app: &mut App) {
    let area = centered(f.area(), 70, 50);
    f.render_widget(Clear, area);

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let items: Vec<ListItem> = app
        .expand_candidates
        .iter()
        .map(|p| {
            let note = if home.as_deref() == Some(p.as_path()) {
                "  (홈)"
            } else if p.parent().is_none() {
                "  (디스크 전체)"
            } else {
                ""
            };
            ListItem::new(Line::from(vec![
                Span::raw(p.display().to_string()),
                Span::styled(note, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 어디까지 미리 스캔할까요?  j/k 이동 · Enter 확인 · Esc 닫기 ")
                .title_bottom(" 넓을수록 시간과 메모리가 더 듭니다. 이미 스캔한 부분은 다시 읽지 않습니다. "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.expand_state);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let spinner = SPINNER[app.tick / 2 % SPINNER.len()];
    let now = Instant::now();

    // 첫 줄: 보여 줄 데이터가 없는 스캔 중이면 스피너와 대상 경로, 아니면 경로와 합계
    let mut line1 = Vec::new();
    match &app.scanning {
        Some(dir) if !app.refreshing => {
            line1.push(Span::raw(format!(" {} 스캔 중… {}", spinner, dir.display())));
        }
        _ => {
            // 이 폴더 데이터를 읽은 시점(일부만 갱신했다면 가장 오래된 부분 기준)과,
            // 같은 스캔이 걸린 시간. 폴더를 오가도 다시 스캔하지 않으므로 이 숫자가
            // 곧 "지금 보는 데이터가 얼마나 오래됐는지"다.
            let took = match app.scanned_at() {
                Some(at) => {
                    let age = age_label(app.now().saturating_sub(at) as u64);
                    match app.scan_duration_at(at) {
                        Some(d) => format!(" · {age} 스캔 · {:.2}초 소요", d.as_secs_f64()),
                        None => format!(" · {age} 스캔"),
                    }
                }
                None => String::new(),
            };
            // 읽지 못한 경로가 있으면 합계는 실제보다 작을 수 있다
            let at_least = if app.unreadable_here() > 0 { "≥ " } else { "" };
            line1.push(Span::raw(format!(
                " {}   (합계: {}{}{})",
                app.current.display(),
                at_least,
                human(app.total_size()),
                took
            )));
            if app.unreadable_here() > 0 {
                line1.push(Span::styled(
                    format!("   ⚠ 읽지 못한 경로 {}건 (e)", app.unreadable_here()),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if app.refreshing {
                let label = match (&app.scanning, app.expanding) {
                    (Some(dir), true) => format!("   {} {} 까지 스캔 중…", spinner, dir.display()),
                    _ => format!("   {} 갱신 중…", spinner),
                };
                line1.push(Span::styled(label, Style::default().fg(Color::Cyan)));
            }
        }
    }
    let mut lines = vec![Line::from(line1)];

    // 둘째 줄(감시 중일 때만): 간격, 다음 갱신, 마지막 확인, 마지막 변화
    if let Some(watch) = &app.watch {
        let mut spans = vec![Span::styled(
            format!(" ⟳ 감시 {}초", watch.secs),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
        if let Some(effective) = app.watch_effective() {
            if effective.as_secs() > watch.secs {
                spans.push(Span::styled(
                    format!(" (스캔이 오래 걸려 {}초)", effective.as_secs()),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        let dim = Style::default().fg(Color::DarkGray);
        match (app.scanning.is_some(), app.watch_next_in(now)) {
            (true, _) => spans.push(Span::styled(" · 갱신 중", dim)),
            (false, Some(left)) => spans.push(Span::styled(
                format!(" · 다음 {}초 후", left.as_secs_f64().ceil() as u64),
                dim,
            )),
            _ => {}
        }
        if let Some(at) = app.last_checked {
            spans.push(Span::styled(format!(" · 확인 {}", ago(now - at)), dim));
        }
        match app.last_change {
            Some((at, delta)) => {
                spans.push(Span::raw(" · 마지막 변화 "));
                spans.push(Span::styled(
                    delta_label(delta),
                    delta_style(delta, now - at < FRESH),
                ));
                spans.push(Span::styled(format!(" ({})", ago(now - at)), dim));
            }
            None => spans.push(Span::styled(" · 아직 변화 없음", dim)),
        }
        spans.push(Span::styled("   [+/-] 간격  [w] 끄기", dim));
        lines.push(Line::from(spans));
    }

    // 변화가 막 감지됐으면 테두리를 잠깐 노랗게 깜빡인다
    let flash = app.last_change.is_some_and(|(at, _)| now - at < FLASH);
    let border = if flash {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if app.watch.is_some() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let header = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(" mac-du "),
    );
    f.render_widget(header, area);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    // 목록: [변화 표시] 용량 · 비율 · 막대 · 이름 · 읽지 못함 · 증감
    let total = app.total_size().max(1);
    let max = app.children().iter().map(|e| e.size).max().unwrap_or(1).max(1);

    let items: Vec<ListItem> = app
        .children()
        .iter()
        .map(|e| {
            let filled = (e.size as f64 / max as f64 * BAR_WIDTH as f64).round() as usize;
            let bar = format!("{}{}", "█".repeat(filled), " ".repeat(BAR_WIDTH - filled));
            let pct = e.size as f64 / total as f64 * 100.0;
            let icon = if e.is_dir { "📁" } else { "📄" };
            let change = app.change_of(&e.name);
            let fresh = change.is_some_and(|c| c.at.elapsed() < FRESH);

            let name_style = if fresh {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                // 막 바뀐 항목은 맨 앞에 점을 찍어 훑어보기만 해도 보이게 한다
                match change {
                    Some(c) if fresh => Span::styled("● ", delta_style(c.delta, true)),
                    _ => Span::raw("  "),
                },
                Span::styled(
                    format!("{:>10} ", human(e.size)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!("{:>5.1}% ", pct),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {} {}", icon, e.name), name_style),
            ];
            // 읽지 못한 부분이 있으면 크기가 실제보다 작다는 표시
            if e.unreadable > 0 {
                spans.push(Span::styled(
                    format!("  ⚠ 읽지 못함 {}", e.unreadable),
                    Style::default().fg(Color::Yellow),
                ));
            }
            if let Some(c) = change {
                spans.push(Span::styled(
                    format!("  {}", change_label(&c)),
                    delta_style(c.delta, fresh),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.state);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // 푸터: 오류 메시지 > 잠깐 보여 주는 알림 > 키 도움말 순서로 보여 준다
    let footer = if let Some(msg) = &app.status {
        Paragraph::new(format!(" ⚠ {}", msg)).style(Style::default().fg(Color::Red))
    } else if let Some(msg) = app.notice() {
        Paragraph::new(format!(" ✓ {}", msg)).style(Style::default().fg(Color::Green))
    } else {
        let items = help_items(app.watch.is_some(), app.unreadable_here() > 0);
        Paragraph::new(help_line(&items, area.width as usize))
            .style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(footer, area);
}

/// 키 도움말 항목: (중요도, 문구). 목록 순서가 화면에 보이는 순서이고,
/// 중요도가 낮은 숫자일수록 먼저 남는다. 0은 폭이 아무리 좁아도 남긴다.
fn help_items(watching: bool, has_errors: bool) -> Vec<(u8, String)> {
    let mut items = vec![
        (1, "j/k 이동".to_string()),
        (1, "l 들어가기".to_string()),
        (1, "h 상위".to_string()),
        (3, "r 갱신".to_string()),
        (6, "p 위쪽".to_string()),
        (5, "w 감시".to_string()),
    ];
    if watching {
        items.push((7, "+/- 간격".to_string()));
    }
    items.push((4, "y/Y 복사".to_string()));
    items.push((4, "o/O Finder".to_string()));
    if has_errors {
        items.push((2, "e 오류".to_string()));
    }
    items.push((0, "q 종료".to_string()));
    items
}

/// `width` 칸 안에 들어가도록 중요한 항목부터 채워서 한 줄로 만든다.
/// 보이는 순서는 `items`의 원래 순서를 따른다.
fn help_line(items: &[(u8, String)], width: usize) -> String {
    const SEP: &str = "  ";
    let avail = width.saturating_sub(1); // 맨 앞 공백 한 칸
    let w = |s: &str| Span::raw(s.to_string()).width();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0); // 같은 중요도는 원래 순서 유지(stable)

    let mut keep = vec![false; items.len()];
    let mut used = 0;
    for i in order {
        let add = w(&items[i].1) + if used == 0 { 0 } else { w(SEP) };
        // 중요도 0은 폭이 모자라도 반드시 남긴다
        if items[i].0 == 0 || used + add <= avail {
            keep[i] = true;
            used += add;
        }
    }

    let shown: Vec<&str> = items
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(it, _)| it.1.as_str())
        .collect();
    format!(" {}", shown.join(SEP))
}

/// 읽지 못한 경로 목록을 화면 가운데 팝업으로 보여 준다.
fn draw_errors(f: &mut Frame, app: &mut App) {
    let area = centered(f.area(), 90, 70);
    f.render_widget(Clear, area);

    let shown = app.error_view.len();
    let total = app.unreadable_here() as usize;
    let title = if total > shown {
        format!(
            " 읽지 못한 경로 {}건 (앞 {}건만 표시) — j/k 이동, Esc 닫기 ",
            total, shown
        )
    } else {
        format!(" 읽지 못한 경로 {}건 — j/k 이동, Esc 닫기 ", total)
    };

    let items: Vec<ListItem> = app
        .error_view
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

/// 증감 색: 디스크 사용량이 늘면 빨강, 줄면 초록. 막 바뀐 것은 굵게.
fn delta_style(delta: i64, fresh: bool) -> Style {
    let color = if delta >= 0 { Color::Red } else { Color::Green };
    let style = Style::default().fg(color);
    if fresh {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// `▲ +12.0 MB` / `▼ -3.0 KB`
fn delta_label(delta: i64) -> String {
    let arrow = if delta >= 0 { "▲ +" } else { "▼ -" };
    format!("{}{}", arrow, human(delta.unsigned_abs()))
}

fn change_label(c: &Change) -> String {
    if c.is_new {
        format!("NEW {}", human(c.delta.unsigned_abs()))
    } else {
        delta_label(c.delta)
    }
}

/// `3초 전` / `2분 전` / `1시간 전` / `3일 전`
fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}초 전", s)
    } else if s < 3600 {
        format!("{}분 전", s / 60)
    } else if s < 86_400 {
        format!("{}시간 전", s / 3600)
    } else {
        format!("{}일 전", s / 86_400)
    }
}

/// 스캔한 지 `secs`초가 지난 데이터의 나이. 5초 안이면 `방금`.
fn age_label(secs: u64) -> String {
    if secs < 5 {
        "방금".to_string()
    } else {
        ago(Duration::from_secs(secs))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_labels_show_direction_and_unit() {
        assert_eq!(delta_label(2048), "▲ +2.0 KB");
        assert_eq!(delta_label(-3 * 1024 * 1024), "▼ -3.0 MB");
        assert_eq!(delta_label(0), "▲ +0 B");
    }

    #[test]
    fn new_entries_are_labelled_new() {
        let c = Change {
            delta: 5 * 1024,
            is_new: true,
            at: Instant::now(),
        };
        assert_eq!(change_label(&c), "NEW 5.0 KB");
    }

    fn items() -> Vec<(u8, String)> {
        [(1, "j/k 이동"), (3, "r 갱신"), (5, "w 감시"), (4, "o/O Finder"), (0, "q 종료")]
            .iter()
            .map(|(p, s)| (*p, s.to_string()))
            .collect()
    }

    #[test]
    fn the_help_line_shows_everything_when_there_is_room() {
        let line = help_line(&items(), 200);
        assert_eq!(line, " j/k 이동  r 갱신  w 감시  o/O Finder  q 종료");
    }

    #[test]
    fn the_help_line_drops_the_least_important_items_first_but_never_the_quit_hint() {
        let full = help_line(&items(), 200);
        let full_width = Span::raw(full).width();

        for width in [full_width - 1, 40, 30, 20, 10, 1] {
            let line = help_line(&items(), width);
            assert!(line.contains("q 종료"), "quit hint missing at width {width}: {line:?}");
            // 폭이 허용하는 한 넘치지 않는다 (q만 남는 극단적으로 좁은 경우는 예외)
            if width >= Span::raw(" q 종료").width() {
                assert!(Span::raw(line.clone()).width() <= width, "too wide at {width}: {line:?}");
            }
        }
        // 한 칸 줄이면 가장 덜 중요한 w 감시(5)부터 빠진다
        let line = help_line(&items(), full_width - 1);
        assert!(!line.contains("w 감시") && line.contains("o/O Finder") && line.contains("r 갱신"));
        // 보이는 순서는 원래 순서 그대로
        assert!(line.find("j/k").unwrap() < line.find("r 갱신").unwrap());
    }

    #[test]
    fn the_real_key_help_fits_a_80_column_terminal_in_every_state() {
        for (watching, has_errors) in [(false, false), (true, false), (false, true), (true, true)] {
            let line = help_line(&help_items(watching, has_errors), 80);
            let width = Span::raw(line.clone()).width();
            assert!(width <= 80, "{width} columns for watching={watching} errors={has_errors}: {line:?}");
            // 이동, 종료, 그리고 새로 넣은 Finder 키는 80칸에서도 남아 있어야 한다
            for must in ["j/k 이동", "l 들어가기", "h 상위", "q 종료", "o/O Finder"] {
                assert!(line.contains(must), "{must:?} missing: {line:?}");
            }
            // 오류가 있으면 e 키 안내가 가장 먼저 남는 부가 항목이다
            if has_errors {
                assert!(line.contains("e 오류"), "{line:?}");
            }
        }
        // 넓은 터미널에서는 전부 보인다
        let wide = help_line(&help_items(true, true), 200);
        for all in ["p 위쪽", "w 감시", "+/- 간격", "y/Y 복사", "e 오류"] {
            assert!(wide.contains(all), "{all:?} missing: {wide:?}");
        }
    }

    #[test]
    fn ago_picks_a_sensible_unit() {
        assert_eq!(ago(Duration::from_secs(4)), "4초 전");
        assert_eq!(ago(Duration::from_secs(125)), "2분 전");
        assert_eq!(ago(Duration::from_secs(7300)), "2시간 전");
        assert_eq!(ago(Duration::from_secs(86_399)), "23시간 전");
        assert_eq!(ago(Duration::from_secs(86_400)), "1일 전");
        assert_eq!(ago(Duration::from_secs(3 * 86_400 + 5)), "3일 전");
    }

    #[test]
    fn data_age_reads_as_just_now_then_counts_up() {
        assert_eq!(age_label(0), "방금");
        assert_eq!(age_label(4), "방금");
        assert_eq!(age_label(5), "5초 전");
        assert_eq!(age_label(59), "59초 전");
        assert_eq!(age_label(60), "1분 전");
        assert_eq!(age_label(3 * 3600), "3시간 전");
        // 시계가 거꾸로 가도(saturating_sub으로 0) 이상한 값이 나오지 않는다
        assert_eq!(age_label(0u32.saturating_sub(10) as u64), "방금");
    }
}

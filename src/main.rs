mod app;
mod cli;
mod clipboard;
mod finder;
mod units;
mod scanner;
mod ui;

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, Jump};

fn main() -> io::Result<()> {
    // 옵션 오류는 터미널을 raw 모드로 바꾸기 전에 처리해야 메시지가 제대로 보인다
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(cli::Parsed::Run(opts)) => opts,
        Ok(cli::Parsed::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Err(msg) => {
            eprintln!("mac-du: {msg}\n자세한 사용법: mac-du --help");
            std::process::exit(2);
        }
    };

    // App::new가 곧바로 스캔을 시작하므로, 그보다 먼저 스레드 풀을 설정해야 한다
    scanner::init_thread_pool(opts.threads);
    // 경로를 준 경우는 `.`, `..`, 상대 경로, 심볼릭 링크를 풀어 실제 경로로 바꾼다.
    // 안 준 경우는 현재 폴더를 그대로 쓴다.
    let start = match &opts.path {
        Some(path) => resolve_dir(path, "경로"),
        None => std::env::current_dir()?,
    };
    let mut app = match &opts.root {
        // 경로 비교가 어긋나지 않도록 둘 다 실제 경로로 맞춘다
        Some(root) => {
            App::with_root(start.canonicalize().unwrap_or(start), resolve_dir(root, "--root 경로"))
        }
        None => App::new(start),
    };
    if opts.si {
        app.units = units::Units::new(units::Base::Decimal);
    }
    if let Some(secs) = opts.watch {
        app.enable_watch(secs);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal, &mut app);

    // 에러가 나도 터미널은 반드시 복구한다
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// 폴더 경로를 실제 경로로 바꾼다. 없거나 폴더가 아니면 터미널을 건드리기 전에 오류로 끝낸다.
fn resolve_dir(path: &Path, label: &str) -> PathBuf {
    match path.canonicalize() {
        Ok(p) if p.is_dir() => p,
        Ok(p) => {
            eprintln!("mac-du: {label}는 폴더여야 합니다: {}", p.display());
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("mac-du: {label}를 열 수 없습니다: {} ({e})", path.display());
            std::process::exit(2);
        }
    }
}

/// 크게 움직이는 키를 `Jump`로 바꾼다: `gg`/Home, `G`/End, Ctrl-d/Ctrl-u, PageDown/PageUp.
/// `gg`는 두 번 눌러야 하므로 앞서 `g`를 눌렀는지를 `pending_g`에 기억한다.
/// 어떤 키든 한 번 지나가면 대기 상태는 풀린다.
fn jump_for(key: &KeyEvent, pending_g: &mut bool, page_rows: usize) -> Option<Jump> {
    let after_g = std::mem::take(pending_g);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = page_rows.saturating_sub(1).max(1);
    let half = (page_rows / 2).max(1);
    match key.code {
        KeyCode::Char('g') if !ctrl => {
            if after_g {
                Some(Jump::Top)
            } else {
                *pending_g = true;
                None
            }
        }
        KeyCode::Home => Some(Jump::Top),
        KeyCode::Char('G') | KeyCode::End => Some(Jump::Bottom),
        KeyCode::Char('d') if ctrl => Some(Jump::Down(half)),
        KeyCode::Char('u') if ctrl => Some(Jump::Up(half)),
        KeyCode::PageDown => Some(Jump::Down(page)),
        KeyCode::PageUp => Some(Jump::Up(page)),
        _ => None,
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    let mut pending_g = false;
    while !app.should_quit {
        app.poll_scan();
        terminal.draw(|f| ui::draw(f, app))?;

        // 입력이 없어도 80ms마다 다시 그려 스캔 결과와 스피너를 반영한다
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let jump = jump_for(&key, &mut pending_g, app.page_rows);
                // 도움말 팝업이 열려 있으면 닫는 키만 받는다
                if app.show_help {
                    if matches!(
                        key.code,
                        KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter
                    ) {
                        app.show_help = false;
                    }
                    continue;
                }
                // 오류 팝업이 열려 있으면 팝업이 키를 모두 가져간다
                if app.show_errors {
                    if let Some(j) = jump {
                        app.error_jump(j);
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('e') | KeyCode::Char('q') | KeyCode::Esc => {
                            app.show_errors = false
                        }
                        KeyCode::Up | KeyCode::Char('k') => app.error_previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.error_next(),
                        _ => {}
                    }
                    continue;
                }
                if app.show_expand {
                    if let Some(j) = jump {
                        app.expand_jump(j);
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('p') | KeyCode::Char('q') | KeyCode::Esc => {
                            app.close_expand()
                        }
                        KeyCode::Up | KeyCode::Char('k') => app.expand_previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.expand_next(),
                        KeyCode::Enter | KeyCode::Char('l') => app.confirm_expand(),
                        _ => {}
                    }
                    continue;
                }
                if let Some(j) = jump {
                    app.jump(j);
                    continue;
                }
                match key.code {
                    KeyCode::Char('?') => app.show_help = true,
                    KeyCode::Char('p') => app.open_expand(),
                    KeyCode::Char('y') => app.copy_selected(),
                    KeyCode::Char('Y') => app.copy_current(),
                    KeyCode::Char('u') => app.cycle_unit(),
                    KeyCode::Char('s') => app.toggle_base(),
                    KeyCode::Char('i') => app.show_exact(),
                    KeyCode::Char('o') => app.reveal_selected(),
                    KeyCode::Char('O') => app.open_current(),
                    KeyCode::Char('e') => app.toggle_errors(),
                    KeyCode::Char('r') => app.rescan(),
                    KeyCode::Char('w') => app.toggle_watch(),
                    // + 는 Shift가 필요하므로 같은 키의 = 도 받는다
                    KeyCode::Char('+') | KeyCode::Char('=') => app.adjust_watch(true),
                    KeyCode::Char('-') => app.adjust_watch(false),
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.enter(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.back(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn plain(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn gg_needs_two_presses_and_any_other_key_cancels_it() {
        let mut g = false;
        assert_eq!(jump_for(&plain('g'), &mut g, 20), None, "one g only waits");
        assert!(g);
        assert_eq!(jump_for(&plain('g'), &mut g, 20), Some(Jump::Top));
        assert!(!g, "the second g is consumed");

        assert_eq!(jump_for(&plain('g'), &mut g, 20), None);
        assert_eq!(jump_for(&plain('j'), &mut g, 20), None);
        assert!(!g, "another key cancels the pending g");
        assert_eq!(jump_for(&plain('g'), &mut g, 20), None, "so the next g starts over");
    }

    #[test]
    fn the_other_jump_keys() {
        let mut g = false;
        let shift_g = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(jump_for(&shift_g, &mut g, 20), Some(Jump::Bottom));
        assert_eq!(jump_for(&key(KeyCode::End, KeyModifiers::NONE), &mut g, 20), Some(Jump::Bottom));
        assert_eq!(jump_for(&key(KeyCode::Home, KeyModifiers::NONE), &mut g, 20), Some(Jump::Top));
        let ctrl_d = key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let ctrl_u = key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(jump_for(&ctrl_d, &mut g, 20), Some(Jump::Down(10)), "half a screen");
        assert_eq!(jump_for(&ctrl_u, &mut g, 20), Some(Jump::Up(10)));
        let pgdn = key(KeyCode::PageDown, KeyModifiers::NONE);
        let pgup = key(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(jump_for(&pgdn, &mut g, 20), Some(Jump::Down(19)), "a page keeps one line of context");
        assert_eq!(jump_for(&pgup, &mut g, 20), Some(Jump::Up(19)));
        // 아주 작은 화면에서도 최소 한 줄은 움직인다
        assert_eq!(jump_for(&pgdn, &mut g, 1), Some(Jump::Down(1)));
        assert_eq!(jump_for(&ctrl_d, &mut g, 1), Some(Jump::Down(1)));
    }

    #[test]
    fn plain_d_and_u_are_not_jumps_so_u_still_cycles_the_unit() {
        let mut g = false;
        assert_eq!(jump_for(&plain('d'), &mut g, 20), None);
        assert_eq!(jump_for(&plain('u'), &mut g, 20), None);
    }
}

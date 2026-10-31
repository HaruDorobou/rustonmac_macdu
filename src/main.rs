mod app;
mod cli;
mod scanner;
mod ui;

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;

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
    let start = std::env::current_dir()?;
    let mut app = match opts.root {
        // 경로 비교가 어긋나지 않도록 심볼릭 링크를 풀어 둘 다 실제 경로로 맞춘다
        Some(root) => match root.canonicalize() {
            Ok(root) if root.is_dir() => {
                App::with_root(start.canonicalize().unwrap_or(start), root)
            }
            Ok(root) => {
                eprintln!("mac-du: --root는 폴더여야 합니다: {}", root.display());
                std::process::exit(2);
            }
            Err(e) => {
                eprintln!("mac-du: --root 경로를 열 수 없습니다: {} ({e})", root.display());
                std::process::exit(2);
            }
        },
        None => App::new(start),
    };
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

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.should_quit {
        app.poll_scan();
        terminal.draw(|f| ui::draw(f, app))?;

        // 입력이 없어도 80ms마다 다시 그려 스캔 결과와 스피너를 반영한다
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // 오류 팝업이 열려 있으면 팝업이 키를 모두 가져간다
                if app.show_errors {
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
                match key.code {
                    KeyCode::Char('p') => app.open_expand(),
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

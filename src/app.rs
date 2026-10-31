use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::scanner::{self, Entry, ErrorReport, ScanOutput};

/// 백그라운드 스캔이 끝났을 때 UI 스레드로 보내는 결과
struct ScanDone {
    dir: PathBuf,
    select: Option<String>,
    result: io::Result<ScanOutput>,
    /// 스캔 스레드 안에서 잰 소요 시간 (UI 렌더링 시간 제외)
    elapsed: Duration,
}

pub struct App {
    /// 현재 보고 있는 디렉토리
    pub current: PathBuf,
    pub entries: Vec<Entry>,
    pub state: ListState,
    pub should_quit: bool,
    /// 이동 실패 등 사용자에게 알릴 메시지
    pub status: Option<String>,
    /// 백그라운드 스캔 진행 중이면 스캔 대상 디렉토리
    pub scanning: Option<PathBuf>,
    /// 스피너 애니메이션용 프레임 카운터
    pub tick: usize,
    /// 현재 화면을 만든 스캔의 소요 시간
    pub last_scan: Option<Duration>,
    /// 현재 화면을 만든 스캔에서 읽지 못한 경로들
    pub errors: ErrorReport,
    /// 오류 목록 팝업이 열려 있는지
    pub show_errors: bool,
    pub error_state: ListState,
    rx: Option<Receiver<ScanDone>>,
}

impl App {
    pub fn new(current: PathBuf) -> Self {
        let mut app = Self {
            current: current.clone(),
            entries: Vec::new(),
            state: ListState::default(),
            should_quit: false,
            status: None,
            scanning: None,
            tick: 0,
            last_scan: None,
            errors: ErrorReport::default(),
            show_errors: false,
            error_state: ListState::default(),
            rx: None,
        };
        app.start_scan(current, None);
        app
    }

    pub fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let i = self.state.selected().map_or(0, |i| (i + 1).min(last));
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(i));
    }

    /// 선택한 항목이 폴더면 그 안으로 들어간다. 파일이거나 스캔 중이면 무시한다.
    pub fn enter(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        let path = match self.selected_entry() {
            Some(e) if e.is_dir => e.path.clone(),
            _ => return,
        };
        self.start_scan(path, None);
    }

    /// 상위 디렉토리로 올라간다. 떠나온 폴더를 다시 선택해 둔다. 스캔 중이면 무시한다.
    pub fn back(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        let Some(parent) = self.current.parent().map(Path::to_path_buf) else {
            return;
        };
        let left_name = self
            .current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.start_scan(parent, left_name);
    }

    /// 현재 폴더를 다시 스캔한다. 권한을 허용해 준 뒤 결과를 갱신할 때 쓴다.
    /// 보던 항목은 그대로 선택해 둔다. 스캔 중이면 무시한다.
    pub fn rescan(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        let selected = self.selected_entry().map(|e| e.name.clone());
        self.start_scan(self.current.clone(), selected);
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// 오류 목록 팝업을 열고 닫는다. 오류가 없으면 열지 않는다.
    pub fn toggle_errors(&mut self) {
        if self.show_errors {
            self.show_errors = false;
        } else if !self.errors.list.is_empty() {
            self.show_errors = true;
        }
    }

    pub fn error_next(&mut self) {
        if self.errors.list.is_empty() {
            return;
        }
        let last = self.errors.list.len() - 1;
        let i = self.error_state.selected().map_or(0, |i| (i + 1).min(last));
        self.error_state.select(Some(i));
    }

    pub fn error_previous(&mut self) {
        if self.errors.list.is_empty() {
            return;
        }
        let i = self.error_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.error_state.select(Some(i));
    }

    /// 매 프레임 호출한다. 스캔이 끝났으면 결과를 반영한다.
    pub fn poll_scan(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok(done) => self.finish_scan(done),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // 스캔 스레드가 결과 없이 종료됨 (패닉 등)
                self.scanning = None;
                self.rx = None;
                self.status = Some("스캔이 중단되었습니다".into());
            }
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    /// `dir`의 스캔을 별도 스레드에서 시작한다.
    fn start_scan(&mut self, dir: PathBuf, select: Option<String>) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.scanning = Some(dir.clone());

        thread::spawn(move || {
            let started = Instant::now();
            let result = scanner::scan_dir(&dir);
            let elapsed = started.elapsed();
            // 수신 쪽이 이미 종료됐다면 보낼 곳이 없을 뿐이므로 무시한다
            let _ = tx.send(ScanDone {
                dir,
                select,
                result,
                elapsed,
            });
        });
    }

    /// 스캔 결과를 반영한다. 실패하면 이전 화면을 그대로 두고 메시지만 남긴다.
    /// `select`가 주어지면 그 이름의 항목을 선택하고, 없으면 첫 항목을 선택한다.
    fn finish_scan(&mut self, done: ScanDone) {
        self.scanning = None;
        self.rx = None;

        match done.result {
            Ok(ScanOutput { entries, errors }) => {
                let index = done
                    .select
                    .as_deref()
                    .and_then(|name| entries.iter().position(|e| e.name == name))
                    .or(if entries.is_empty() { None } else { Some(0) });
                self.current = done.dir;
                self.entries = entries;
                self.state.select(index);
                self.error_state
                    .select(if errors.list.is_empty() { None } else { Some(0) });
                self.errors = errors;
                self.show_errors = false;
                self.last_scan = Some(done.elapsed);
                self.status = None;
            }
            Err(e) => {
                self.status = Some(format!("열 수 없음: {} ({})", done.dir.display(), e));
            }
        }
    }
}

use std::io;
use std::path::{Path, PathBuf};

use ratatui::widgets::ListState;

use crate::scanner::{self, Entry};

pub struct App {
    /// 현재 보고 있는 디렉토리
    pub current: PathBuf,
    pub entries: Vec<Entry>,
    pub state: ListState,
    pub should_quit: bool,
    /// 이동 실패 등 사용자에게 알릴 메시지
    pub status: Option<String>,
}

impl App {
    pub fn new(current: PathBuf) -> io::Result<Self> {
        let entries = scanner::scan_dir(&current)?;
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        Ok(Self {
            current,
            entries,
            state,
            should_quit: false,
            status: None,
        })
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

    /// 선택한 항목이 폴더면 그 안으로 들어간다. 파일이면 무시한다.
    pub fn enter(&mut self) {
        let path = match self.selected_entry() {
            Some(e) if e.is_dir => e.path.clone(),
            _ => return,
        };
        self.go_to(path, None);
    }

    /// 상위 디렉토리로 올라간다. 떠나온 폴더를 다시 선택해 둔다.
    pub fn back(&mut self) {
        let Some(parent) = self.current.parent().map(Path::to_path_buf) else {
            return;
        };
        let left_name = self
            .current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.go_to(parent, left_name.as_deref());
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    /// `dir`을 스캔해 현재 디렉토리로 바꾼다.
    /// 실패하면 상태는 그대로 두고 메시지만 남긴다.
    /// `select`가 주어지면 그 이름의 항목을 선택하고, 없으면 첫 항목을 선택한다.
    fn go_to(&mut self, dir: PathBuf, select: Option<&str>) {
        match scanner::scan_dir(&dir) {
            Ok(entries) => {
                let index = select
                    .and_then(|name| entries.iter().position(|e| e.name == name))
                    .or(if entries.is_empty() { None } else { Some(0) });
                self.current = dir;
                self.entries = entries;
                self.state.select(index);
                self.status = None;
            }
            Err(e) => {
                self.status = Some(format!("열 수 없음: {} ({})", dir.display(), e));
            }
        }
    }
}

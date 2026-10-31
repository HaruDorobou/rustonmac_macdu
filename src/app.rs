use std::path::PathBuf;

use ratatui::widgets::ListState;

use crate::scanner::Entry;

pub struct App {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub state: ListState,
    pub should_quit: bool,
}

impl App {
    pub fn new(root: PathBuf, entries: Vec<Entry>) -> Self {
        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }
        Self {
            root,
            entries,
            state,
            should_quit: false,
        }
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

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

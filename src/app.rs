use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::scanner::{self, ErrorReport, Node, ScanError, ScanOutput};

/// 스캔이 끝나면 결과를 어디에 반영할지
enum Target {
    /// 트리 전체를 새로 만든다 (시작할 때, 또는 트리 루트보다 위로 올라갈 때)
    NewRoot { select: Option<String> },
    /// 트리 안의 한 폴더만 다시 스캔해 끼워 넣는다
    Subtree,
}

/// 백그라운드 스캔이 끝났을 때 UI 스레드로 보내는 결과
struct ScanDone {
    dir: PathBuf,
    target: Target,
    result: io::Result<ScanOutput>,
    /// 스캔 스레드 안에서 잰 소요 시간 (UI 렌더링 시간 제외)
    elapsed: Duration,
}

pub struct App {
    /// 스캔해 둔 트리의 루트 폴더 (시작한 폴더, 또는 거기서 위로 올라간 폴더)
    tree_root: PathBuf,
    root: Node,
    /// 트리 루트에서 현재 폴더까지 내려온 자식 인덱스들. 비어 있으면 루트를 보는 중이다.
    stack: Vec<usize>,
    /// 현재 보고 있는 디렉토리
    pub current: PathBuf,
    pub state: ListState,
    pub should_quit: bool,
    /// 이동 실패 등 사용자에게 알릴 메시지
    pub status: Option<String>,
    /// 백그라운드 스캔 진행 중이면 스캔 대상 디렉토리
    pub scanning: Option<PathBuf>,
    /// 스피너 애니메이션용 프레임 카운터
    pub tick: usize,
    /// 트리를 만든(또는 마지막으로 갱신한) 스캔의 소요 시간
    pub last_scan: Option<Duration>,
    /// 트리 전체에서 읽지 못한 경로들
    errors: ErrorReport,
    /// 현재 폴더 하위의 읽지 못한 경로들 (팝업에 보여 줄 목록)
    pub error_view: Vec<ScanError>,
    /// 오류 목록 팝업이 열려 있는지
    pub show_errors: bool,
    pub error_state: ListState,
    rx: Option<Receiver<ScanDone>>,
}

impl App {
    pub fn new(current: PathBuf) -> Self {
        let mut app = Self {
            tree_root: current.clone(),
            root: Node::empty(),
            stack: Vec::new(),
            current: current.clone(),
            state: ListState::default(),
            should_quit: false,
            status: None,
            scanning: None,
            tick: 0,
            last_scan: None,
            errors: ErrorReport::default(),
            error_view: Vec::new(),
            show_errors: false,
            error_state: ListState::default(),
            rx: None,
        };
        app.start_scan(current, Target::NewRoot { select: None });
        app
    }

    /// 지금 보고 있는 폴더의 노드
    pub fn current_node(&self) -> &Node {
        let mut node = &self.root;
        for &i in &self.stack {
            node = &node.children[i];
        }
        node
    }

    pub fn children(&self) -> &[Node] {
        &self.current_node().children
    }

    pub fn total_size(&self) -> u64 {
        self.current_node().size
    }

    /// 현재 폴더 하위에서 읽지 못한 경로의 수
    pub fn unreadable_here(&self) -> u32 {
        self.current_node().unreadable
    }

    pub fn next(&mut self) {
        let len = self.children().len();
        if len == 0 {
            return;
        }
        let i = self.state.selected().map_or(0, |i| (i + 1).min(len - 1));
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.children().is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(i));
    }

    /// 선택한 항목이 폴더면 그 안으로 들어간다. 이미 스캔해 둔 트리에서 바로 보여 주므로
    /// 다시 스캔하지 않는다. 파일이면 무시한다.
    pub fn enter(&mut self) {
        let Some(i) = self.state.selected() else {
            return;
        };
        let Some(child) = self.current_node().children.get(i) else {
            return;
        };
        if !child.is_dir {
            return;
        }
        if child.children.is_empty() && child.unreadable > 0 {
            let msg = format!("읽지 못한 폴더입니다: {}", child.name);
            self.status = Some(msg);
            return;
        }
        let name = child.name.to_string();
        let has_children = !child.children.is_empty();

        self.stack.push(i);
        self.current.push(name);
        self.state.select(has_children.then_some(0));
        self.status = None;
        self.show_errors = false;
    }

    /// 상위 폴더로 올라간다. 떠나온 폴더를 다시 선택해 둔다.
    /// 트리 루트보다 위로 올라갈 때만 부모 폴더를 새로 스캔한다.
    pub fn back(&mut self) {
        if let Some(i) = self.stack.pop() {
            self.current.pop();
            self.state.select(Some(i));
            self.status = None;
            self.show_errors = false;
            return;
        }
        if self.scanning.is_some() {
            return;
        }
        let Some(parent) = self.tree_root.parent().map(Path::to_path_buf) else {
            return;
        };
        let left_name = self
            .tree_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.start_scan(parent, Target::NewRoot { select: left_name });
    }

    /// 현재 폴더 하위만 다시 스캔해 트리에 끼워 넣는다. 권한을 허용해 준 뒤 결과를
    /// 갱신할 때 쓴다. 이미 스캔 중이면 무시한다.
    pub fn rescan(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        self.start_scan(self.current.clone(), Target::Subtree);
    }

    /// 오류 목록 팝업을 열고 닫는다. 현재 폴더 하위에 읽지 못한 경로가 없으면 열지 않는다.
    pub fn toggle_errors(&mut self) {
        if self.show_errors {
            self.show_errors = false;
            return;
        }
        self.error_view = self
            .errors
            .list
            .iter()
            .filter(|e| e.path.starts_with(&self.current))
            .cloned()
            .collect();
        if !self.error_view.is_empty() {
            self.error_state.select(Some(0));
            self.show_errors = true;
        }
    }

    pub fn error_next(&mut self) {
        if self.error_view.is_empty() {
            return;
        }
        let last = self.error_view.len() - 1;
        let i = self.error_state.selected().map_or(0, |i| (i + 1).min(last));
        self.error_state.select(Some(i));
    }

    pub fn error_previous(&mut self) {
        if self.error_view.is_empty() {
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

    /// `dir`의 스캔을 별도 스레드에서 시작한다.
    fn start_scan(&mut self, dir: PathBuf, target: Target) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.scanning = Some(dir.clone());

        thread::spawn(move || {
            let started = Instant::now();
            let result = scanner::scan_tree(&dir);
            let elapsed = started.elapsed();
            // 수신 쪽이 이미 종료됐다면 보낼 곳이 없을 뿐이므로 무시한다
            let _ = tx.send(ScanDone {
                dir,
                target,
                result,
                elapsed,
            });
        });
    }

    /// 스캔 결과를 반영한다. 실패하면 이전 화면을 그대로 두고 메시지만 남긴다.
    fn finish_scan(&mut self, done: ScanDone) {
        self.scanning = None;
        self.rx = None;

        let out = match done.result {
            Ok(out) => out,
            Err(e) => {
                self.status = Some(format!("열 수 없음: {} ({})", done.dir.display(), e));
                return;
            }
        };
        self.status = None;

        match done.target {
            Target::NewRoot { select } => {
                self.root = out.root;
                self.errors = out.errors;
                self.tree_root = done.dir.clone();
                self.stack.clear();
                self.current = done.dir;
                let index = select
                    .as_deref()
                    .and_then(|name| self.children().iter().position(|c| &*c.name == name))
                    .or(if self.children().is_empty() { None } else { Some(0) });
                self.state.select(index);
                self.error_view.clear();
                self.show_errors = false;
            }
            Target::Subtree => {
                let Some(names) = self.names_under_root(&done.dir) else {
                    return;
                };
                let removed = self.node_at(&names).map_or(0, |n| n.unreadable as usize);
                let selected = self.selected_name();
                if !self.root.replace_at(&names, out.root) {
                    // 스캔하는 사이 상위 폴더 구조가 바뀌었다
                    self.status = Some("폴더 구조가 바뀌어 갱신하지 못했습니다".into());
                    return;
                }
                self.errors.replace_under(&done.dir, removed, out.errors);
                self.reindex(selected);
                self.error_view.clear();
                self.show_errors = false;
            }
        }

        self.last_scan = Some(done.elapsed);
    }

    /// `dir`이 트리 루트 아래(또는 루트 자신)일 때, 루트에서 거기까지의 폴더 이름들
    fn names_under_root(&self, dir: &Path) -> Option<Vec<String>> {
        let rel = dir.strip_prefix(&self.tree_root).ok()?;
        Some(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect(),
        )
    }

    fn node_at(&self, names: &[String]) -> Option<&Node> {
        let mut node = &self.root;
        for name in names {
            node = node.children.iter().find(|c| c.is_dir && *c.name == **name)?;
        }
        Some(node)
    }

    fn selected_name(&self) -> Option<String> {
        let i = self.state.selected()?;
        self.children().get(i).map(|n| n.name.to_string())
    }

    /// 트리가 바뀐 뒤(정렬 순서가 달라졌을 수 있다) 현재 위치와 선택을 이름으로 다시 찾는다.
    fn reindex(&mut self, selected: Option<String>) {
        let names = self.names_under_root(&self.current).unwrap_or_default();

        let mut stack = Vec::new();
        let mut node = &self.root;
        for name in &names {
            match node.children.iter().position(|c| c.is_dir && *c.name == **name) {
                Some(i) => {
                    stack.push(i);
                    node = &node.children[i];
                }
                None => break,
            }
        }
        // 보던 폴더가 사라졌다면 남아 있는 가장 깊은 상위 폴더로 물러난다
        if stack.len() < names.len() {
            self.status = Some("보던 폴더가 사라져 상위 폴더로 이동했습니다".into());
        }
        let mut current = self.tree_root.clone();
        let mut walk = &self.root;
        for &i in &stack {
            walk = &walk.children[i];
            current.push(&*walk.name);
        }
        self.stack = stack;
        self.current = current;

        let prev = self.state.selected().unwrap_or(0);
        let children = self.children();
        let index = selected
            .as_deref()
            .and_then(|name| children.iter().position(|c| &*c.name == name))
            .or_else(|| (!children.is_empty()).then(|| prev.min(children.len() - 1)));
        self.state.select(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mac-du-app-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn wait(app: &mut App) {
        for _ in 0..500 {
            app.poll_scan();
            if app.scanning.is_none() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("scan did not finish in time");
    }

    fn names(app: &App) -> Vec<String> {
        app.children().iter().map(|c| c.name.to_string()).collect()
    }

    #[test]
    fn navigation_uses_the_cached_tree_without_rescanning() {
        let root = temp_root("nav");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 300_000]).unwrap();
        fs::write(root.join("c/f"), vec![1u8; 1_000]).unwrap();

        let mut app = App::new(root.clone());
        wait(&mut app);
        assert_eq!(names(&app), ["a", "c"]);

        app.enter(); // a
        assert!(app.scanning.is_none(), "enter must not start a scan");
        assert_eq!(app.current, root.join("a"));
        assert_eq!(names(&app), ["b"]);

        app.enter(); // a/b
        assert_eq!(app.current, root.join("a/b"));
        assert_eq!(names(&app), ["f"]);

        app.back();
        app.back();
        assert!(app.scanning.is_none(), "going up inside the tree must not scan");
        assert_eq!(app.current, root);
        assert_eq!(app.state.selected(), Some(0)); // 떠나온 a가 다시 선택됨

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rescan_refreshes_only_the_current_subtree_and_keeps_position() {
        let root = temp_root("rescan");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("c/f"), vec![1u8; 100_000]).unwrap();

        let mut app = App::new(root.clone());
        wait(&mut app);
        assert_eq!(names(&app), ["c", "a"]); // c가 더 크다

        app.state.select(Some(1)); // a
        app.enter();
        app.enter(); // a/b
        assert_eq!(app.current, root.join("a/b"));

        // 밖에서 a/b에 큰 파일이 생겼다. 현재 폴더만 다시 스캔한다.
        fs::write(root.join("a/b/big"), vec![1u8; 500_000]).unwrap();
        app.rescan();
        wait(&mut app);

        assert_eq!(app.current, root.join("a/b"));
        assert!(names(&app).contains(&"big".to_string()));

        // 위로 올라가 보면 a가 커져서 c보다 앞에 오고, 합계도 맞아야 한다
        app.back();
        app.back();
        assert_eq!(names(&app), ["a", "c"]);
        assert_eq!(app.state.selected(), Some(0));
        let sum: u64 = app.children().iter().map(|c| c.size).sum();
        assert_eq!(app.total_size(), sum);
        assert!(app.total_size() >= 600_000);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn going_above_the_tree_root_scans_the_parent() {
        let root = temp_root("up");
        fs::create_dir_all(root.join("x")).unwrap();
        fs::create_dir_all(root.join("y")).unwrap();
        fs::write(root.join("x/f"), vec![1u8; 1_000]).unwrap();
        fs::write(root.join("y/f"), vec![1u8; 100_000]).unwrap();

        let mut app = App::new(root.join("x"));
        wait(&mut app);
        app.back(); // 트리 루트(x)보다 위 → 부모를 새로 스캔
        assert!(app.scanning.is_some());
        wait(&mut app);

        assert_eq!(app.current, root);
        assert_eq!(names(&app), ["y", "x"]);
        let selected = app.state.selected().unwrap();
        assert_eq!(&*app.children()[selected].name, "x"); // 떠나온 폴더가 선택됨

        fs::remove_dir_all(&root).unwrap();
    }
}

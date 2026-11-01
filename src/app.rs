use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::clipboard;
use crate::finder;
use crate::scanner::{self, ErrorReport, Node, ScanError, ScanOutput};
use crate::units::{self, Units};

/// 감시 간격의 최솟값(초). 이보다 짧으면 스캔이 겹치거나 디스크를 계속 읽게 된다.
pub const MIN_WATCH_SECS: u64 = 2;
/// `w`로 처음 켤 때의 감시 간격(초)
pub const DEFAULT_WATCH_SECS: u64 = 5;
/// `+`/`-` 키로 오르내리는 간격 단계(초). 마지막 단계를 넘으면 두 배씩 늘어난다(상한 없음).
const WATCH_STEPS: [u64; 12] = [2, 3, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600];
/// 크기 변화 표시를 화면에 남겨 두는 시간
const CHANGE_VISIBLE: Duration = Duration::from_secs(30);
/// "복사됨" 같은 알림을 푸터에 남겨 두는 시간
const NOTICE_VISIBLE: Duration = Duration::from_secs(3);
/// 이 깊이 이하의 폴더(`/`는 0, `/Users`는 1, `/Users/이름`은 2)를 감시하면 경고한다
const WIDE_WATCH_DEPTH: usize = 2;

/// 감시 간격을 한 단계 늘린다.
pub fn step_up(secs: u64) -> u64 {
    WATCH_STEPS
        .iter()
        .copied()
        .find(|&s| s > secs)
        .unwrap_or(secs.saturating_mul(2))
}

/// 감시 간격을 한 단계 줄인다. `MIN_WATCH_SECS` 아래로는 내려가지 않는다.
pub fn step_down(secs: u64) -> u64 {
    WATCH_STEPS
        .iter()
        .rev()
        .copied()
        .find(|&s| s < secs)
        .unwrap_or(MIN_WATCH_SECS)
        .max(MIN_WATCH_SECS)
}

/// `root`의 상위 폴더들을 가까운 순으로 돌려준다 (`/`까지). `/`이면 비어 있다.
fn expand_candidates_for(root: &Path) -> Vec<PathBuf> {
    root.ancestors().skip(1).map(Path::to_path_buf).collect()
}

/// 스캔이 끝나면 결과를 어디에 반영할지
enum Target {
    /// 트리 전체를 새로 만든다 (시작할 때, 또는 트리 루트보다 위로 올라갈 때).
    /// `open`이 있으면 스캔이 끝난 뒤 그 폴더로 들어가 있는다 (`--root` 사용 시 시작 폴더).
    NewRoot {
        select: Option<String>,
        open: Option<PathBuf>,
    },
    /// 트리 안의 한 폴더만 다시 스캔해 끼워 넣는다
    Subtree,
    /// 트리 루트보다 위의 폴더(`ScanDone::dir`)까지 범위를 넓힌다. 이미 스캔한 기존 트리는
    /// 읽지 않고 그대로 끼워 넣으므로 위쪽 나머지 부분만 새로 읽는다.
    /// 끝나면 `open`이 있을 때만 그 폴더로 이동하고(`h`로 올라가는 경우), 없으면 사용자가
    /// 그 사이 옮겨 간 위치를 그대로 둔다. `select`가 없으면 보던 항목을 유지한다.
    Expand {
        open: Option<PathBuf>,
        select: Option<String>,
    },
}

/// 백그라운드 스캔이 끝났을 때 UI 스레드로 보내는 결과
struct ScanDone {
    dir: PathBuf,
    target: Target,
    result: io::Result<ScanOutput>,
    /// 이 스캔의 시작 시각. 읽은 노드마다 기록된 `scanned`와 같은 값이다.
    stamp: u32,
    /// 스캔 스레드 안에서 잰 소요 시간 (UI 렌더링 시간 제외)
    elapsed: Duration,
}

/// 감시 모드: 일정 간격으로 현재 폴더를 다시 스캔한다.
pub struct Watch {
    /// 사용자가 정한 간격(초)
    pub secs: u64,
    next_at: Instant,
}

/// 직전 스캔과 비교한 항목 하나의 크기 변화
#[derive(Debug, Clone, Copy)]
pub struct Change {
    /// 바이트 단위 증감 (새 항목이면 그 크기)
    pub delta: i64,
    pub is_new: bool,
    pub at: Instant,
}

#[derive(Default)]
struct Changes {
    /// 이 변화들이 속한 폴더. 다른 폴더를 보고 있을 때는 표시하지 않는다.
    dir: PathBuf,
    map: HashMap<String, Change>,
}

/// 목록 정렬 순서. 스캔 결과는 항상 크기 큰 순으로 저장되고, 보고 있는 폴더만 이 순서로 다시 늘어놓는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    SizeDesc,
    Name,
    SizeAsc,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            SortMode::SizeDesc => SortMode::Name,
            SortMode::Name => SortMode::SizeAsc,
            SortMode::SizeAsc => SortMode::SizeDesc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortMode::SizeDesc => "크기 큰 순",
            SortMode::Name => "이름순",
            SortMode::SizeAsc => "크기 작은 순",
        }
    }
}

/// 목록 안에서 한 번에 크게 움직이는 방법 (vim의 gg, G, Ctrl-d, Ctrl-u 등)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jump {
    Top,
    Bottom,
    Down(usize),
    Up(usize),
}

/// 항목이 `len`개인 목록에서 `selected`를 `jump`만큼 옮긴 위치. 목록이 비어 있으면 `None`.
/// 끝을 넘어가면 처음이나 끝에서 멈춘다 (돌아 나오지 않는다).
fn jumped(selected: Option<usize>, len: usize, jump: Jump) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cur = selected.unwrap_or(0).min(len - 1);
    Some(match jump {
        Jump::Top => 0,
        Jump::Bottom => len - 1,
        Jump::Down(n) => cur.saturating_add(n).min(len - 1),
        Jump::Up(n) => cur.saturating_sub(n),
    })
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
    /// 진행 중인 스캔이 기존 화면을 유지한 채 하는 갱신(`r`, 감시)인지.
    /// 시작/상위 이동처럼 보여 줄 데이터가 없는 스캔이면 `false`.
    pub refreshing: bool,
    /// 진행 중인 스캔이 상위 폴더로 범위를 넓히는 것인지 (`refreshing`의 한 종류)
    pub expanding: bool,
    /// 상위 폴더 미리 스캔 팝업이 열려 있는지
    pub show_expand: bool,
    /// 팝업에 보여 줄 후보: 트리 루트의 상위 폴더들 (가까운 순)
    pub expand_candidates: Vec<PathBuf>,
    pub expand_state: ListState,
    /// 스피너 애니메이션용 프레임 카운터
    pub tick: usize,
    /// 가장 최근에 끝난 스캔의 소요 시간
    pub last_scan: Option<Duration>,
    /// 트리 전체에서 읽지 못한 경로들
    errors: ErrorReport,
    /// 현재 폴더 하위의 읽지 못한 경로들 (팝업에 보여 줄 목록)
    pub error_view: Vec<ScanError>,
    /// 오류 목록 팝업이 열려 있는지
    pub show_errors: bool,
    pub error_state: ListState,
    /// 지금 보고 있는 폴더의 정렬 순서
    pub sort: SortMode,
    /// 검색어를 입력하는 중이면 지금까지 친 글자
    pub search_input: Option<String>,
    /// 검색을 시작하기 전에 선택돼 있던 위치. `Esc`로 취소하면 여기로 돌아간다.
    search_origin: Option<usize>,
    /// 마지막으로 확정한 검색어. `n`/`N`이 이것을 다시 찾는다.
    last_query: String,
    /// 경고 팝업의 내용. 있으면 팝업이 열려 있고, 확인할 때까지 다른 키를 받지 않는다.
    pub warning: Option<String>,
    /// 키 도움말 팝업이 열려 있는지
    pub show_help: bool,
    /// 지금 가장 위에 보이는 목록이 한 화면에 보여 주는 줄 수. 그릴 때마다 갱신된다.
    /// PageUp/PageDown과 Ctrl-d/Ctrl-u가 얼마나 움직일지 정하는 데 쓴다.
    pub page_rows: usize,

    pub watch: Option<Watch>,
    /// 감시를 껐다 켜도 마지막에 쓴 간격을 기억한다
    last_watch_secs: u64,
    /// 마지막으로 스캔 결과를 확인한 시각 (변화가 없어도 갱신된다)
    pub last_checked: Option<Instant>,
    /// 마지막으로 현재 폴더 합계가 달라진 시각과 증감(바이트)
    pub last_change: Option<(Instant, i64)>,
    /// 크기를 어떤 기준과 단위로 보여 줄지
    pub units: Units,
    /// 오류가 아닌 짧은 알림 (복사됨 등). 일정 시간 뒤에는 보이지 않는다.
    /// 완료한 동작이면 문구가 `✓`로 시작한다.
    notice: Option<(String, Instant)>,
    /// 텍스트를 클립보드에 넣는 함수. 테스트가 실제 클립보드를 덮어쓰지 않도록 바꿔 끼울 수 있다.
    copier: Box<dyn FnMut(&str) -> io::Result<()>>,
    /// `open`을 부르는 함수. 테스트가 Finder 창을 띄우지 않도록 바꿔 끼울 수 있다.
    opener: Box<dyn FnMut(&[&str]) -> io::Result<()>>,
    /// 현재 시각(유닉스 초)을 돌려주는 함수. 테스트가 시각을 정할 수 있게 바꿔 끼울 수 있다.
    clock: Box<dyn Fn() -> u32>,
    /// 스캔 시작 시각별 소요 시간. 헤더의 "N분 전 스캔 · N초 소요"를 같은 스캔 기준으로 맞춘다.
    scan_durations: Vec<(u32, Duration)>,
    changes: Changes,
    rx: Option<Receiver<ScanDone>>,
}

impl App {
    /// `current`를 루트로 스캔하고 거기서 시작한다.
    pub fn new(current: PathBuf) -> Self {
        Self::with_root(current.clone(), current)
    }

    /// `root` 전체를 미리 스캔해 두고 `start`에서 시작한다. 그러면 `root` 아래에서는 어느
    /// 폴더로 오가도 다시 스캔하지 않는다. `start`가 `root` 밖이면 `root`에서 시작한다.
    pub fn with_root(start: PathBuf, root: PathBuf) -> Self {
        Self::build(start, root, Box::new(scanner::now_secs))
    }

    fn build(start: PathBuf, root: PathBuf, clock: Box<dyn Fn() -> u32>) -> Self {
        let open = start.starts_with(&root).then_some(start);
        let mut app = Self {
            tree_root: root.clone(),
            root: Node::empty(),
            stack: Vec::new(),
            current: root.clone(),
            state: ListState::default(),
            should_quit: false,
            status: None,
            scanning: None,
            refreshing: false,
            expanding: false,
            show_expand: false,
            expand_candidates: Vec::new(),
            expand_state: ListState::default(),
            tick: 0,
            last_scan: None,
            errors: ErrorReport::default(),
            error_view: Vec::new(),
            show_errors: false,
            error_state: ListState::default(),
            show_help: false,
            warning: None,
            sort: SortMode::default(),
            search_input: None,
            search_origin: None,
            last_query: String::new(),
            page_rows: 10,
            watch: None,
            last_watch_secs: DEFAULT_WATCH_SECS,
            last_checked: None,
            last_change: None,
            units: Units::default(),
            notice: None,
            copier: Box::new(clipboard::copy),
            opener: Box::new(finder::open),
            clock,
            scan_durations: Vec::new(),
            changes: Changes::default(),
            rx: None,
        };
        app.start_scan(root, Target::NewRoot { select: None, open });
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

    fn current_node_mut(&mut self) -> &mut Node {
        let mut node = &mut self.root;
        for &i in &self.stack {
            node = &mut node.children[i];
        }
        node
    }

    /// `S`: 정렬 순서를 크기 큰 순 → 이름순 → 크기 작은 순으로 바꾼다.
    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.apply_sort();
        self.notice = Some((format!("정렬: {}", self.sort.label()), Instant::now()));
    }

    /// 지금 보고 있는 폴더를 `sort` 순서로 늘어놓고, 하이라이트는 같은 항목에 둔다.
    /// 상위 폴더들은 건드리지 않는다. (스택이 부모 안의 위치를 기억하므로 현재 폴더 안의 순서만 바꾼다.)
    /// 다른 폴더는 들어가거나 돌아올 때 이 함수로 맞춘다.
    fn apply_sort(&mut self) {
        let name = self.selected_name();
        let mode = self.sort;
        let children = &mut self.current_node_mut().children;
        match mode {
            SortMode::SizeDesc => children.sort_by(|a, b| b.size.cmp(&a.size)),
            SortMode::SizeAsc => children.sort_by(|a, b| a.size.cmp(&b.size)),
            SortMode::Name => children.sort_by_cached_key(|c| c.name.to_lowercase()),
        }
        if let Some(name) = name {
            let i = self.children().iter().position(|c| *c.name == *name);
            self.state.select(i.or(Some(0)).filter(|_| !self.children().is_empty()));
        }
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

    /// 목록에서 선택을 크게 옮긴다.
    pub fn jump(&mut self, jump: Jump) {
        let i = jumped(self.state.selected(), self.children().len(), jump);
        self.state.select(i);
    }

    // ---- 검색 -------------------------------------------------------------

    /// 현재 폴더 목록에서 `query`가 이름에 들어 있는(대소문자 무시) 항목의 위치.
    /// `start`에서 `forward` 방향으로 훑고, 끝에 닿으면 반대쪽 끝에서 이어간다.
    /// `include_start`가 `false`면 `start` 자신은 마지막에 본다.
    fn find_match(&self, query: &str, start: usize, forward: bool, include_start: bool) -> Option<usize> {
        let len = self.children().len();
        if len == 0 || query.is_empty() {
            return None;
        }
        let q = query.to_lowercase();
        let first = usize::from(!include_start);
        (first..first + len).find_map(|off| {
            let i = if forward { (start + off) % len } else { (start + len - off % len) % len };
            self.children()[i].name.to_lowercase().contains(&q).then_some(i)
        })
    }

    /// `/`: 검색어 입력을 시작한다.
    pub fn start_search(&mut self) {
        if self.children().is_empty() {
            return;
        }
        self.search_origin = self.state.selected();
        self.search_input = Some(String::new());
        self.status = None;
    }

    /// 입력 중인 검색어를 고치고, 지금 위치부터 처음 일치하는 항목으로 하이라이트를 옮긴다.
    pub fn search_edit(&mut self, f: impl FnOnce(&mut String)) {
        let Some(mut q) = self.search_input.take() else { return };
        f(&mut q);
        let origin = self.search_origin.unwrap_or(0);
        match self.find_match(&q, origin, true, true) {
            Some(i) => self.state.select(Some(i)),
            None => self.state.select(self.search_origin),
        }
        self.search_input = Some(q);
    }

    /// 입력 중인 검색어에 일치하는 항목이 있는가 (비어 있으면 `true`)
    pub fn search_has_match(&self) -> bool {
        let Some(q) = &self.search_input else { return true };
        let origin = self.search_origin.unwrap_or(0);
        q.is_empty() || self.find_match(q, origin, true, true).is_some()
    }

    /// `Enter`: 지금 하이라이트된 위치에서 검색을 끝내고 검색어를 기억해 둔다.
    pub fn search_confirm(&mut self) {
        let Some(q) = self.search_input.take() else { return };
        if q.is_empty() {
            return;
        }
        if self.find_match(&q, self.search_origin.unwrap_or(0), true, true).is_none() {
            self.state.select(self.search_origin);
            self.status = Some(format!("일치하는 항목이 없습니다: {q}"));
        }
        self.last_query = q;
    }

    /// `Esc`: 검색을 취소하고 시작하기 전 위치로 돌아간다.
    pub fn search_cancel(&mut self) {
        if self.search_input.take().is_some() {
            self.state.select(self.search_origin);
        }
    }

    /// `n` / `N`: 마지막 검색어와 일치하는 다음 / 이전 항목으로 간다.
    pub fn search_next(&mut self, forward: bool) {
        if self.last_query.is_empty() {
            self.status = Some("검색어가 없습니다. / 로 먼저 검색하세요".into());
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        match self.find_match(&self.last_query, cur, forward, false) {
            Some(i) => self.state.select(Some(i)),
            None => self.status = Some(format!("일치하는 항목이 없습니다: {}", self.last_query)),
        }
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
        self.apply_sort();
        // 정렬 순서가 크기순이 아니면 맨 위 항목이 달라졌을 수 있다
        if has_children {
            self.state.select(Some(0));
        }
        self.status = None;
        self.show_errors = false;
    }

    /// 상위 폴더로 올라간다. 떠나온 폴더를 다시 선택해 둔다.
    /// 트리 루트보다 위로 올라갈 때만 부모 폴더를 새로 스캔한다.
    pub fn back(&mut self) {
        if let Some(i) = self.stack.pop() {
            self.current.pop();
            self.state.select(Some(i));
            // 안에 있는 동안 정렬 순서를 바꿨을 수 있으므로 돌아온 폴더도 맞춘다
            self.apply_sort();
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
        // 이미 스캔한 트리는 버리지 않고 부모의 나머지(형제 폴더들)만 새로 읽는다
        self.start_scan(
            parent.clone(),
            Target::Expand {
                open: Some(parent),
                select: left_name,
            },
        );
    }

    /// 현재 폴더 하위만 다시 스캔해 트리에 끼워 넣는다. 권한을 허용해 준 뒤나 파일이
    /// 바뀐 뒤 결과를 갱신할 때 쓴다. 이미 스캔 중이면 무시한다.
    pub fn rescan(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        self.start_scan(self.current.clone(), Target::Subtree);
    }

    // ---- 감시(watch) ------------------------------------------------------

    /// 감시 모드를 `secs`초 간격으로 켠다. 최솟값 미만이면 최솟값으로 올린다.
    pub fn enable_watch(&mut self, secs: u64) {
        let secs = secs.max(MIN_WATCH_SECS);
        self.last_watch_secs = secs;
        self.watch = Some(Watch {
            secs,
            next_at: Instant::now(),
        });
        self.schedule_next();
    }

    pub fn toggle_watch(&mut self) {
        if self.watch.is_some() {
            self.watch = None;
        } else {
            self.enable_watch(self.last_watch_secs);
            self.warn_if_wide_watch();
        }
    }

    /// 루트에 가까운 넓은 폴더를 감시하면 간격마다 그 아래 전체를 다시 읽으므로 경고한다.
    /// 감시는 막지 않는다. 느리다는 것을 알고 쓰는 경우도 있기 때문이다.
    pub fn warn_if_wide_watch(&mut self) {
        if self.watch.is_none() {
            return;
        }
        let depth = self
            .current
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .count();
        if depth <= WIDE_WATCH_DEPTH {
            self.warning = Some(format!(
                "넓은 폴더를 감시하고 있습니다.\n\n  {}\n\n감시는 {}초마다 이 폴더 아래 전체를 다시 읽습니다.\n스캔이 오래 걸리는 폴더면 디스크와 CPU를 계속 많이 씁니다.\n끄려면 w를 누르세요.",
                self.current.display(),
                self.watch.as_ref().map_or(0, |w| w.secs),
            ));
        }
    }

    /// 감시 간격을 한 단계 늘리거나(`true`) 줄인다. 감시가 꺼져 있으면 무시한다.
    pub fn adjust_watch(&mut self, longer: bool) {
        let Some(w) = &self.watch else { return };
        let secs = if longer {
            step_up(w.secs)
        } else {
            step_down(w.secs)
        };
        self.enable_watch(secs);
    }

    /// 실제로 적용되는 감시 간격. 큰 폴더라 스캔이 오래 걸리면 디스크를 계속 읽지 않도록
    /// 마지막 스캔 시간의 2배까지 간격을 늘린다.
    pub fn watch_effective(&self) -> Option<Duration> {
        let w = self.watch.as_ref()?;
        let took = self.last_scan.map_or(Duration::ZERO, |d| d * 2);
        Some(Duration::from_secs(w.secs).max(took))
    }

    /// 다음 감시 스캔까지 남은 시간
    pub fn watch_next_in(&self, now: Instant) -> Option<Duration> {
        self.watch
            .as_ref()
            .map(|w| w.next_at.saturating_duration_since(now))
    }

    /// 지금 감시 스캔을 시작할 때인지. 스캔 중에는 겹치지 않게 항상 `false`.
    pub fn watch_due(&self, now: Instant) -> bool {
        self.scanning.is_none() && self.watch.as_ref().is_some_and(|w| now >= w.next_at)
    }

    /// 스캔이 하나 끝날 때마다 다음 감시 시각을 "끝난 시점부터" 다시 잡는다.
    fn schedule_next(&mut self) {
        let Some(effective) = self.watch_effective() else {
            return;
        };
        if let Some(w) = &mut self.watch {
            w.next_at = Instant::now() + effective;
        }
    }

    /// 방금 갱신에서 `name` 항목의 크기가 달라졌다면 그 변화 (일정 시간 동안만)
    pub fn change_of(&self, name: &str) -> Option<Change> {
        if self.changes.dir != self.current {
            return None;
        }
        self.changes
            .map
            .get(name)
            .copied()
            .filter(|c| c.at.elapsed() < CHANGE_VISIBLE)
    }

    // ---- 상위 폴더 미리 스캔 ----------------------------------------------

    /// 상위 폴더 중 하나를 골라 그 폴더까지 미리 스캔해 두는 팝업을 연다.
    /// 홈 디렉토리가 후보에 있으면 그것을 먼저 선택해 둔다.
    pub fn open_expand(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        let candidates = expand_candidates_for(&self.tree_root);
        if candidates.is_empty() {
            self.status = Some("이미 최상위 폴더(/)까지 스캔했습니다".into());
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let preferred = home
            .and_then(|h| candidates.iter().position(|c| *c == h))
            .unwrap_or(0);
        self.expand_candidates = candidates;
        self.expand_state.select(Some(preferred));
        self.show_expand = true;
        self.show_errors = false;
    }

    pub fn close_expand(&mut self) {
        self.show_expand = false;
    }

    pub fn expand_next(&mut self) {
        if self.expand_candidates.is_empty() {
            return;
        }
        let last = self.expand_candidates.len() - 1;
        let i = self.expand_state.selected().map_or(0, |i| (i + 1).min(last));
        self.expand_state.select(Some(i));
    }

    pub fn expand_jump(&mut self, jump: Jump) {
        let i = jumped(self.expand_state.selected(), self.expand_candidates.len(), jump);
        self.expand_state.select(i);
    }

    pub fn expand_previous(&mut self) {
        if self.expand_candidates.is_empty() {
            return;
        }
        let i = self.expand_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.expand_state.select(Some(i));
    }

    /// 고른 상위 폴더까지 스캔을 시작한다. 보고 있는 화면은 그대로 두고, 끝나면
    /// 새 트리로 바꾼다. 이미 스캔한 부분은 다시 읽지 않는다.
    pub fn confirm_expand(&mut self) {
        let Some(dir) = self
            .expand_state
            .selected()
            .and_then(|i| self.expand_candidates.get(i))
            .cloned()
        else {
            return;
        };
        self.show_expand = false;
        if self.scanning.is_some() {
            return;
        }
        self.start_scan(
            dir,
            Target::Expand {
                open: None,
                select: None,
            },
        );
    }

    // ---- 크기 표시 --------------------------------------------------------

    /// 단위를 자동 → K → M → G → T 순으로 바꾼다.
    pub fn cycle_unit(&mut self) {
        self.units.cycle_unit();
    }

    /// 1024 기준과 1000 기준(Finder와 같음)을 바꾼다.
    pub fn toggle_base(&mut self) {
        self.units.toggle_base();
    }

    /// 하이라이트된 항목의 정확한 바이트 수와, 두 기준으로 읽은 값을 푸터에 잠깐 보여 준다.
    pub fn show_exact(&mut self) {
        let Some(node) = self
            .state
            .selected()
            .and_then(|i| self.current_node().children.get(i))
        else {
            self.status = Some("표시할 항목이 없습니다".into());
            return;
        };
        // 읽지 못한 부분이 있으면 실제 크기는 이 값 이상이다 (헤더의 `≥`와 같은 뜻)
        let at_least = if node.unreadable > 0 { "≥ " } else { "" };
        let msg = format!("크기: {at_least}{}", units::exact(node.size));
        self.status = None;
        self.notice = Some((msg, Instant::now()));
    }

    // ---- 경로 복사 --------------------------------------------------------

    /// 하이라이트된 항목의 전체 경로
    pub fn selected_path(&self) -> Option<PathBuf> {
        Some(self.current.join(self.selected_name()?))
    }

    /// 하이라이트된 항목의 전체 경로를 클립보드에 복사한다.
    pub fn copy_selected(&mut self) {
        match self.selected_path() {
            Some(path) => self.copy_text(path.to_string_lossy().into_owned()),
            None => self.status = Some("복사할 항목이 없습니다".into()),
        }
    }

    /// 지금 보고 있는 폴더의 경로를 클립보드에 복사한다.
    pub fn copy_current(&mut self) {
        let text = self.current.to_string_lossy().into_owned();
        self.copy_text(text);
    }

    fn copy_text(&mut self, text: String) {
        match (self.copier)(&text) {
            Ok(()) => {
                self.status = None;
                self.notice = Some((format!("✓ 복사됨: {text}"), Instant::now()));
            }
            Err(e) => {
                self.notice = None;
                self.status = Some(format!("클립보드에 복사하지 못했습니다: {e}"));
            }
        }
    }

    /// 푸터에 잠깐 보여 줄 알림. 시간이 지나면 `None`.
    pub fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_VISIBLE)
            .map(|(msg, _)| msg.as_str())
    }

    // ---- Finder -----------------------------------------------------------

    /// 하이라이트된 항목을 Finder에서 선택된 상태로 보여 준다 (`open -R`).
    pub fn reveal_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            self.status = Some("열 항목이 없습니다".into());
            return;
        };
        // Finder는 `.`으로 시작하는 항목을 기본으로 보여 주지 않는다
        let hidden = self.selected_name().is_some_and(|n| n.starts_with('.'));
        let text = path.to_string_lossy().into_owned();
        self.run_open(&["-R", &text], format!("✓ Finder에서 보기: {text}"), hidden);
    }

    /// 지금 보고 있는 폴더를 Finder에서 연다 (`open`).
    pub fn open_current(&mut self) {
        let text = self.current.to_string_lossy().into_owned();
        self.run_open(&[&text], format!("✓ Finder에서 열기: {text}"), false);
    }

    fn run_open(&mut self, args: &[&str], done: String, hidden: bool) {
        match (self.opener)(args) {
            Ok(()) => {
                self.status = None;
                let msg = if hidden {
                    format!("{done}  (숨김 항목입니다. Finder에서 ⌘⇧. 로 표시)")
                } else {
                    done
                };
                self.notice = Some((msg, Instant::now()));
            }
            Err(e) => {
                self.notice = None;
                self.status = Some(format!("Finder를 열지 못했습니다: {e}"));
            }
        }
    }

    // ---- 오류 목록 --------------------------------------------------------

    /// 오류 목록 팝업을 열고 닫는다. 현재 폴더 하위에 읽지 못한 경로가 없으면 열지 않는다.
    pub fn toggle_errors(&mut self) {
        if self.show_errors {
            self.show_errors = false;
            return;
        }
        self.refresh_error_view();
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

    pub fn error_jump(&mut self, jump: Jump) {
        let i = jumped(self.error_state.selected(), self.error_view.len(), jump);
        self.error_state.select(i);
    }

    pub fn error_previous(&mut self) {
        if self.error_view.is_empty() {
            return;
        }
        let i = self.error_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.error_state.select(Some(i));
    }

    /// 현재 폴더 하위의 오류 목록을 다시 만든다. 팝업이 열려 있으면 스크롤 위치를 유지하고,
    /// 보여 줄 오류가 없어졌으면 닫는다.
    fn refresh_error_view(&mut self) {
        self.error_view = self
            .errors
            .list
            .iter()
            .filter(|e| e.path.starts_with(&self.current))
            .cloned()
            .collect();
        if self.error_view.is_empty() {
            self.show_errors = false;
            self.error_state.select(None);
        } else {
            let i = self.error_state.selected().unwrap_or(0);
            self.error_state.select(Some(i.min(self.error_view.len() - 1)));
        }
    }

    // ---- 스캔 -------------------------------------------------------------

    /// 매 프레임 호출한다. 감시 스캔을 시작할 때가 되었으면 시작하고,
    /// 스캔이 끝났으면 결과를 반영한다.
    pub fn poll_scan(&mut self) {
        self.tick = self.tick.wrapping_add(1);

        if self.watch_due(Instant::now()) {
            self.rescan();
        }
        if self.tick % 100 == 0 {
            self.changes.map.retain(|_, c| c.at.elapsed() < CHANGE_VISIBLE);
        }

        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok(done) => self.finish_scan(done),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // 스캔 스레드가 결과 없이 종료됨 (패닉 등)
                self.scanning = None;
                self.refreshing = false;
                self.expanding = false;
                self.rx = None;
                self.status = Some("스캔이 중단되었습니다".into());
                self.schedule_next();
            }
        }
    }

    /// `dir`의 스캔을 별도 스레드에서 시작한다.
    fn start_scan(&mut self, dir: PathBuf, target: Target) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.scanning = Some(dir.clone());
        // 기존 화면이 있는 채로 하는 스캔이면 화면을 유지한다 (시작/--root 스캔만 제외)
        self.refreshing = !matches!(target, Target::NewRoot { .. });
        self.expanding = matches!(target, Target::Expand { .. });
        // 범위를 넓힐 때는 이미 스캔한 현재 트리를 다시 읽지 않도록 건너뛴다
        let skip = self.expanding.then(|| self.tree_root.clone());
        let stamp = (self.clock)();

        thread::spawn(move || {
            let started = Instant::now();
            let result = scanner::scan_tree_skipping(&dir, skip.as_deref(), stamp);
            let elapsed = started.elapsed();
            // 수신 쪽이 이미 종료됐다면 보낼 곳이 없을 뿐이므로 무시한다
            let _ = tx.send(ScanDone {
                dir,
                target,
                result,
                stamp,
                elapsed,
            });
        });
    }

    fn finish_scan(&mut self, done: ScanDone) {
        self.scanning = None;
        self.refreshing = false;
        self.expanding = false;
        self.rx = None;
        self.apply_scan(done);
        self.schedule_next();
    }

    /// 스캔 결과를 반영한다. 실패하면 이전 화면을 그대로 두고 메시지만 남긴다.
    fn apply_scan(&mut self, done: ScanDone) {
        let out = match done.result {
            Ok(out) => out,
            Err(e) => {
                self.status = Some(format!("열 수 없음: {} ({})", done.dir.display(), e));
                return;
            }
        };
        self.status = None;

        match done.target {
            Target::NewRoot { select, open } => {
                self.root = out.root;
                self.errors = out.errors;
                self.tree_root = done.dir.clone();
                self.stack.clear();
                self.current = done.dir;
                match open {
                    // 미리 스캔해 둔 트리 안의 시작 폴더로 내려간다
                    Some(open) => {
                        self.current = open;
                        self.reindex(select);
                    }
                    None => {
                        let index = select
                            .as_deref()
                            .and_then(|name| {
                                self.children().iter().position(|c| &*c.name == name)
                            })
                            .or(if self.children().is_empty() { None } else { Some(0) });
                        self.state.select(index);
                    }
                }
                self.changes = Changes::default();
                self.last_change = None;
                self.error_view.clear();
                self.show_errors = false;
            }
            Target::Expand { open, select } => {
                // 기존 트리가 새 트리의 어디에 들어가는지 (새 루트 → 기존 루트까지의 폴더 이름들)
                let Ok(rel) = self.tree_root.strip_prefix(&done.dir) else {
                    return;
                };
                let names: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                // 자리표시 노드가 없으면(스캔하는 사이 폴더가 사라졌다면) 기존 트리를 지키기 위해 중단
                if out.root.find(&names).is_none() {
                    self.status = Some("폴더 구조가 바뀌어 범위를 넓히지 못했습니다".into());
                    return;
                }
                let selected = select.or_else(|| self.selected_name());
                let old_root = std::mem::replace(&mut self.root, out.root);
                let grafted = self.root.replace_at(&names, old_root);
                debug_assert!(grafted, "placeholder existence was checked above");

                let mut errors = out.errors;
                errors.absorb(std::mem::take(&mut self.errors));
                self.errors = errors;
                self.tree_root = done.dir.clone();
                if let Some(open) = open {
                    self.current = open;
                }
                self.reindex(selected);
                self.refresh_error_view();
            }
            Target::Subtree => {
                let Some(names) = self.names_under_root(&done.dir) else {
                    return;
                };
                let removed = self.node_at(&names).map_or(0, |n| n.unreadable as usize);
                let selected = self.selected_name();
                // 갱신 전의 현재 폴더 상태. 갱신 후와 비교해 무엇이 달라졌는지 알아낸다.
                let before_dir = self.current.clone();
                let before_total = self.total_size();
                let before_sizes: HashMap<String, u64> = self
                    .children()
                    .iter()
                    .map(|c| (c.name.to_string(), c.size))
                    .collect();

                if !self.root.replace_at(&names, out.root) {
                    // 스캔하는 사이 상위 폴더 구조가 바뀌었다
                    self.status = Some("폴더 구조가 바뀌어 갱신하지 못했습니다".into());
                    return;
                }
                self.errors.replace_under(&done.dir, removed, out.errors);
                self.reindex(selected);
                if self.current == before_dir {
                    self.record_changes(&before_sizes, before_total);
                }
                self.refresh_error_view();
            }
        }

        self.apply_sort();
        self.last_scan = Some(done.elapsed);
        self.last_checked = Some(Instant::now());
        self.scan_durations.push((done.stamp, done.elapsed));
        // 오래된 기록은 버린다. 화면에 남아 있는 데이터가 참조하는 스캔은 최근 것들뿐이다.
        if self.scan_durations.len() > 64 {
            self.scan_durations.remove(0);
        }
    }

    /// 지금 시각(유닉스 초)
    pub fn now(&self) -> u32 {
        (self.clock)()
    }

    /// 지금 보고 있는 폴더의 목록을 읽은 스캔의 시작 시각. 아래 폴더 일부를 따로 다시 스캔했거나
    /// 범위를 넓히며 이전 결과를 재사용했더라도, 이 폴더를 읽은 스캔 하나만 가리킨다.
    /// (소요 시간과 같은 스캔에서 가져와야 서로 맞는 숫자가 된다.) 아직 스캔하기 전이면 `None`.
    pub fn scanned_at(&self) -> Option<u32> {
        let t = self.current_node().scanned;
        (t != 0 && t != u32::MAX).then_some(t)
    }

    /// 시작 시각이 `stamp`인 스캔이 걸린 시간
    pub fn scan_duration_at(&self, stamp: u32) -> Option<Duration> {
        self.scan_durations
            .iter()
            .rev()
            .find(|(s, _)| *s == stamp)
            .map(|(_, d)| *d)
    }

    /// 갱신 전후의 현재 폴더 항목 크기를 비교해 달라진 것을 기록한다.
    fn record_changes(&mut self, before: &HashMap<String, u64>, before_total: u64) {
        let now = Instant::now();
        let mut map = if self.changes.dir == self.current {
            std::mem::take(&mut self.changes.map)
        } else {
            HashMap::new()
        };

        for c in self.children() {
            let change = match before.get(&*c.name) {
                Some(&old) if old != c.size => Change {
                    delta: c.size as i64 - old as i64,
                    is_new: false,
                    at: now,
                },
                None => Change {
                    delta: c.size as i64,
                    is_new: true,
                    at: now,
                },
                _ => continue,
            };
            map.insert(c.name.to_string(), change);
        }
        self.changes = Changes {
            dir: self.current.clone(),
            map,
        };

        let total_delta = self.total_size() as i64 - before_total as i64;
        if total_delta != 0 {
            self.last_change = Some((now, total_delta));
        }
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
        assert!(app.refreshing, "a rescan keeps the old data on screen");
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
        app.back(); // 트리 루트(x)보다 위 → 부모까지 범위를 넓힌다
        assert!(app.scanning.is_some());
        assert!(app.refreshing && app.expanding, "the old screen stays while expanding");
        assert_eq!(app.current, root.join("x"), "still showing x until the scan is done");
        wait(&mut app);

        assert_eq!(app.current, root);
        assert_eq!(names(&app), ["y", "x"]);
        let selected = app.state.selected().unwrap();
        assert_eq!(&*app.children()[selected].name, "x"); // 떠나온 폴더가 선택됨

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn expanding_reuses_the_subtree_it_already_has() {
        let root = temp_root("reuse");
        fs::create_dir_all(root.join("x")).unwrap();
        fs::create_dir_all(root.join("y")).unwrap();
        fs::write(root.join("x/f"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("y/f"), vec![1u8; 100_000]).unwrap();

        let mut app = App::new(root.join("x"));
        wait(&mut app);
        let x_before = app.total_size();

        // 스캔 뒤에 x가 바뀌었다. 위로 올라가며 x를 다시 읽는다면 이 변화가 보일 것이다.
        fs::write(root.join("x/huge"), vec![1u8; 900_000]).unwrap();
        app.back();
        wait(&mut app);

        assert_eq!(app.current, root);
        let x = app.children().iter().find(|c| &*c.name == "x").unwrap();
        assert_eq!(x.size, x_before, "x must come from the tree we already had");

        // 합계는 새로 읽은 y와 재사용한 x가 맞게 합쳐져야 한다
        let sum: u64 = app.children().iter().map(|c| c.size).sum();
        assert_eq!(app.total_size(), sum);

        // r로 갱신하면 그제야 x의 변화가 반영된다
        app.state.select(Some(app.children().iter().position(|c| &*c.name == "x").unwrap()));
        app.enter();
        app.rescan();
        wait(&mut app);
        assert!(names(&app).contains(&"huge".to_string()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_expand_popup_widens_the_cache_without_moving_the_view() {
        let root = temp_root("popup");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("z")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("z/big"), vec![1u8; 300_000]).unwrap();

        let start = root.join("a/b");
        let mut app = App::new(start.clone());
        wait(&mut app);

        app.open_expand();
        assert!(app.show_expand);
        // 후보는 시작 폴더의 상위 폴더들이고 가까운 순이다
        assert_eq!(app.expand_candidates[0], root.join("a"));
        assert_eq!(app.expand_candidates[1], root);

        let pos = app.expand_candidates.iter().position(|c| *c == root).unwrap();
        app.expand_state.select(Some(pos));
        app.confirm_expand();
        assert!(!app.show_expand);
        assert!(app.expanding && app.refreshing);
        assert_eq!(app.current, start, "the view stays put while scanning");
        wait(&mut app);

        // 화면 위치는 그대로, 하지만 이제 위쪽과 형제 폴더가 전부 캐시에 있다
        assert_eq!(app.current, start);
        assert_eq!(names(&app), ["f"]);
        app.back();
        app.back();
        assert!(app.scanning.is_none(), "going up is instant now");
        assert_eq!(app.current, root);
        assert_eq!(names(&app), ["z", "a"]);

        fs::remove_dir_all(&root).unwrap();
    }

    /// 시각을 테스트가 정하는 앱. 반환된 `Cell`을 바꾸면 이후 스캔이 그 시각으로 기록된다.
    fn app_with_clock(
        start: &Path,
        root: &Path,
        now: u32,
    ) -> (App, std::rc::Rc<std::cell::Cell<u32>>) {
        let clock = std::rc::Rc::new(std::cell::Cell::new(now));
        let c = clock.clone();
        let app = App::build(start.to_path_buf(), root.to_path_buf(), Box::new(move || c.get()));
        (app, clock)
    }

    #[test]
    fn the_scan_time_follows_the_data_through_partial_refreshes() {
        let root = temp_root("age");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("c/f"), vec![1u8; 100_000]).unwrap();

        let (mut app, clock) = app_with_clock(&root, &root, 1000);
        assert_eq!(app.scanned_at(), None, "nothing is shown before the first scan finishes");
        wait(&mut app);
        assert_eq!(app.scanned_at(), Some(1000));
        assert!(app.scan_duration_at(1000).is_some(), "the scan's duration is kept by its start time");

        // 시각이 지난 뒤, 폴더를 오가기만 하면 시각은 그대로다 (다시 읽지 않으므로)
        clock.set(2000);
        let a = names(&app).iter().position(|n| n == "a").unwrap();
        app.state.select(Some(a));
        app.enter();
        app.enter(); // a/b
        assert_eq!(app.scanned_at(), Some(1000), "moving around does not refresh anything");
        assert_eq!(app.now(), 2000);

        // a/b만 다시 읽으면 그 폴더는 2000, 위쪽은 여전히 가장 오래된 1000이다
        app.rescan();
        wait(&mut app);
        assert_eq!(app.scanned_at(), Some(2000));
        assert!(app.scan_duration_at(2000).is_some());
        app.back(); // a: 목록은 1000에 읽었고 b만 2000
        assert_eq!(app.scanned_at(), Some(1000));
        app.back(); // 루트
        assert_eq!(app.scanned_at(), Some(1000));
        let a_node = app.children().iter().find(|c| &*c.name == "a").unwrap();
        assert_eq!((a_node.scanned, a_node.oldest), (1000, 1000));

        // 루트를 통째로 다시 읽으면 전부 3000이 된다
        clock.set(3000);
        app.rescan();
        wait(&mut app);
        assert_eq!(app.scanned_at(), Some(3000));
        assert!(app.children().iter().all(|c| c.oldest == 3000));
        // 이전 스캔들의 소요 시간도 시각으로 찾을 수 있다
        assert!(app.scan_duration_at(1000).is_some() && app.scan_duration_at(2000).is_some());
        assert!(app.scan_duration_at(1234).is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn widening_the_cache_keeps_the_old_subtree_at_its_old_time() {
        let root = temp_root("age-expand");
        fs::create_dir_all(root.join("x")).unwrap();
        fs::create_dir_all(root.join("y")).unwrap();
        fs::write(root.join("x/f"), vec![1u8; 1_000]).unwrap();
        fs::write(root.join("y/f"), vec![1u8; 100_000]).unwrap();

        let (mut app, clock) = app_with_clock(&root.join("x"), &root.join("x"), 1000);
        wait(&mut app);
        assert_eq!(app.scanned_at(), Some(1000));

        clock.set(5000);
        app.back(); // x 위로: 부모를 읽되 x는 재사용
        wait(&mut app);

        assert_eq!(app.current, root);
        let age = |name: &str| app.children().iter().find(|c| &*c.name == name).unwrap().oldest;
        assert_eq!(age("x"), 1000, "x was reused, so it keeps the time it was read");
        assert_eq!(age("y"), 5000, "y was read just now");
        assert_eq!(app.scanned_at(), Some(5000), "the folder itself was listed by the newer scan");
        assert!(app.scan_duration_at(1000).is_some() && app.scan_duration_at(5000).is_some());
        assert!(app.scan_duration_at(app.scanned_at().unwrap()).is_some());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn watching_a_shallow_folder_warns_but_still_watches() {
        let root = temp_root("wide-watch");
        fs::create_dir_all(root.join("x")).unwrap();
        let mut app = App::new(root.clone());
        wait(&mut app);

        // 임시 폴더는 깊은 경로이므로 경고가 없다
        app.toggle_watch();
        assert!(app.watch.is_some());
        assert_eq!(app.warning, None);
        app.toggle_watch();

        // 깊이 0, 1, 2는 경고하고, 3부터는 하지 않는다
        for (path, warns) in [("/", true), ("/Users", true), ("/Users/me", true), ("/Users/me/work", false)] {
            app.watch = None;
            app.warning = None;
            app.current = PathBuf::from(path);
            app.toggle_watch();
            assert!(app.watch.is_some(), "{path}: the warning does not stop the watch");
            assert_eq!(app.warning.is_some(), warns, "{path}");
        }
        // 간격만 바꿀 때는 다시 경고하지 않는다
        app.warning = None;
        app.current = PathBuf::from("/");
        app.adjust_watch(true);
        assert_eq!(app.warning, None);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_moves_the_highlight_confirms_cancels_and_repeats() {
        let root = temp_root("search");
        // 크기 순서: big_Alpha(6) > beta(5) > alpha_two(4) > gamma(3) > Delta(2) > alpha_three(1)
        for (i, name) in ["big_Alpha", "beta", "alpha_two", "gamma", "Delta", "alpha_three"].iter().enumerate() {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("f"), vec![1u8; 100_000 * (6 - i)]).unwrap();
        }
        let mut app = App::new(root.clone());
        wait(&mut app);
        assert_eq!(names(&app), ["big_Alpha", "beta", "alpha_two", "gamma", "Delta", "alpha_three"]);

        // 치는 동안 따라간다 (대소문자 무시, 이름 일부)
        app.state.select(Some(1)); // beta
        app.start_search();
        app.search_edit(|q| q.push_str("alp"));
        assert_eq!(app.state.selected(), Some(2), "first match at or after where we were");
        assert!(app.search_has_match());
        app.search_edit(|q| q.push_str("ha_t"));
        assert_eq!(app.state.selected(), Some(2));
        app.search_edit(|q| q.push_str("hree"));
        assert_eq!(app.state.selected(), Some(5));

        // Esc는 시작 위치로 돌려놓고 검색어를 기억하지 않는다
        app.search_cancel();
        assert_eq!(app.state.selected(), Some(1));
        assert!(app.search_input.is_none());
        app.search_next(true);
        assert!(app.status.is_some(), "nothing to repeat yet");

        // Enter는 그 자리에서 확정하고, n / N은 끝에서 처음으로 이어서 돌아본다
        app.state.select(Some(1));
        app.start_search();
        app.search_edit(|q| q.push_str("alpha"));
        app.search_confirm();
        assert_eq!(app.state.selected(), Some(2), "first match at or after where we started");
        app.search_next(true);
        assert_eq!(app.state.selected(), Some(5));
        app.search_next(true);
        assert_eq!(app.state.selected(), Some(0), "wraps past the end; big_Alpha matches case-insensitively");
        app.search_next(false);
        assert_eq!(app.state.selected(), Some(5), "N goes backwards, wrapping past the top");
        app.search_next(false);
        assert_eq!(app.state.selected(), Some(2));

        // 일치가 없으면 하이라이트는 움직이지 않고, 확정하면 알려 준다
        app.state.select(Some(3));
        app.start_search();
        app.search_edit(|q| q.push_str("zzz"));
        assert!(!app.search_has_match());
        assert_eq!(app.state.selected(), Some(3));
        app.search_confirm();
        assert_eq!(app.state.selected(), Some(3));
        assert!(app.status.as_deref().is_some_and(|m| m.contains("zzz")));

        // 한 글자씩 지우면 다시 처음 위치부터 찾는다
        app.state.select(Some(3));
        app.start_search();
        app.search_edit(|q| q.push_str("dx"));
        app.search_edit(|q| {
            q.pop();
        });
        assert_eq!(app.state.selected(), Some(4), "Delta");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorting_reorders_only_the_view_and_follows_the_highlight_through_navigation() {
        let root = temp_root("sort");
        // 크기: zeta(4) > Mid(3) > alpha(2) > beta(1). 안쪽 폴더 zeta 안에는 y(1) < x(5)
        for (name, kb) in [("zeta", 40), ("Mid", 30), ("alpha", 20), ("beta", 10)] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("f"), vec![1u8; kb * 1000]).unwrap();
        }
        fs::write(root.join("zeta/y"), vec![1u8; 100]).unwrap();
        fs::write(root.join("zeta/x"), vec![1u8; 200_000]).unwrap();
        let mut app = App::new(root.clone());
        wait(&mut app);
        assert_eq!(app.sort, SortMode::SizeDesc);
        assert_eq!(names(&app), ["zeta", "Mid", "alpha", "beta"]);

        // 하이라이트한 항목(Mid)은 순서가 바뀌어도 그대로 따라간다
        app.state.select(Some(1));
        app.cycle_sort();
        assert_eq!(app.sort, SortMode::Name);
        assert_eq!(names(&app), ["alpha", "beta", "Mid", "zeta"], "case-insensitive by name");
        assert_eq!(app.selected_name().as_deref(), Some("Mid"));
        app.cycle_sort();
        assert_eq!(names(&app), ["beta", "alpha", "Mid", "zeta"]);
        assert_eq!(app.selected_name().as_deref(), Some("Mid"));

        // 안으로 들어가도 같은 순서를 쓰고, 맨 위 항목이 선택된다
        let zeta = names(&app).iter().position(|n| n == "zeta").unwrap();
        app.state.select(Some(zeta));
        app.enter();
        assert_eq!(app.sort, SortMode::SizeAsc);
        let inner: Vec<_> = names(&app);
        let sizes: Vec<u64> = app.children().iter().map(|c| c.size).collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{inner:?} {sizes:?}");
        assert_eq!(app.state.selected(), Some(0));

        // 안에서 순서를 바꾼 뒤 돌아오면 위쪽도 새 순서이고, 나온 폴더(zeta)에 하이라이트가 있다
        app.cycle_sort(); // 크기 큰 순
        app.cycle_sort(); // 이름순
        assert_eq!(app.sort, SortMode::Name);
        app.back();
        assert_eq!(names(&app), ["alpha", "beta", "Mid", "zeta"]);
        assert_eq!(app.selected_name().as_deref(), Some("zeta"));

        // 다시 읽어도 현재 정렬 순서를 지킨다
        app.rescan();
        wait(&mut app);
        assert_eq!(names(&app), ["alpha", "beta", "Mid", "zeta"]);
        assert_eq!(app.selected_name().as_deref(), Some("zeta"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn jumping_stops_at_both_ends_and_tolerates_empty_lists() {
        assert_eq!(jumped(Some(3), 10, Jump::Top), Some(0));
        assert_eq!(jumped(Some(3), 10, Jump::Bottom), Some(9));
        assert_eq!(jumped(Some(3), 10, Jump::Down(4)), Some(7));
        assert_eq!(jumped(Some(3), 10, Jump::Down(100)), Some(9), "does not run past the end");
        assert_eq!(jumped(Some(3), 10, Jump::Up(100)), Some(0), "does not run past the top");
        assert_eq!(jumped(None, 10, Jump::Down(2)), Some(2), "nothing selected counts as the top");
        assert_eq!(jumped(Some(50), 10, Jump::Up(1)), Some(8), "a stale selection is clamped first");
        assert_eq!(jumped(Some(0), 0, Jump::Bottom), None);
        assert_eq!(jumped(None, 0, Jump::Top), None);
    }

    #[test]
    fn jump_moves_the_highlight_in_the_main_list() {
        let root = temp_root("jump");
        for n in 0..6 {
            fs::create_dir_all(root.join(format!("d{n}"))).unwrap();
            fs::write(root.join(format!("d{n}/f")), vec![1u8; 1_000 * (n + 1)]).unwrap();
        }
        let mut app = App::new(root.clone());
        wait(&mut app);

        app.jump(Jump::Bottom);
        assert_eq!(app.state.selected(), Some(5));
        app.jump(Jump::Up(2));
        assert_eq!(app.state.selected(), Some(3));
        app.jump(Jump::Top);
        assert_eq!(app.state.selected(), Some(0));
        app.jump(Jump::Down(99));
        assert_eq!(app.state.selected(), Some(5));

        fs::remove_dir_all(&root).unwrap();
    }

    /// 실제 클립보드를 건드리지 않도록 복사 함수를 기록용으로 바꾼 앱
    fn app_with_recording_clipboard(
        dir: &Path,
    ) -> (App, std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
        let copied = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut app = App::new(dir.to_path_buf());
        let sink = copied.clone();
        app.copier = Box::new(move |text| {
            sink.borrow_mut().push(text.to_string());
            Ok(())
        });
        wait(&mut app);
        (app, copied)
    }

    #[test]
    fn y_copies_the_highlighted_path_and_capital_y_the_current_folder() {
        let root = temp_root("copy");
        // 공백과 한글이 든 이름도 그대로 복사되어야 한다
        fs::write(root.join("내 큰 파일 (1).bin"), vec![1u8; 300_000]).unwrap();
        fs::create_dir_all(root.join("sub dir")).unwrap();
        fs::write(root.join("sub dir/inner.txt"), vec![1u8; 10_000]).unwrap();

        let (mut app, copied) = app_with_recording_clipboard(&root);
        assert_eq!(names(&app), ["내 큰 파일 (1).bin", "sub dir"]);
        assert!(app.notice().is_none());

        app.copy_selected(); // 첫 항목: 큰 파일
        assert_eq!(copied.borrow()[0], root.join("내 큰 파일 (1).bin").to_string_lossy());
        assert!(app.notice().unwrap().contains("복사됨"));
        assert!(app.status.is_none());

        app.copy_current();
        assert_eq!(copied.borrow()[1], root.to_string_lossy());

        // 폴더 안에서는 그 폴더 기준의 전체 경로가 복사된다
        app.state.select(Some(1));
        app.enter();
        app.copy_current();
        app.copy_selected();
        assert_eq!(copied.borrow()[2], root.join("sub dir").to_string_lossy());
        assert_eq!(copied.borrow()[3], root.join("sub dir/inner.txt").to_string_lossy());
        assert_eq!(copied.borrow().len(), 4);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copying_with_nothing_highlighted_or_a_failing_clipboard_reports_an_error() {
        let root = temp_root("copy-err");
        fs::create_dir_all(root.join("empty")).unwrap();

        let (mut app, copied) = app_with_recording_clipboard(&root);
        app.enter(); // 빈 폴더
        assert_eq!(app.current, root.join("empty"));
        app.copy_selected();
        assert!(app.status.is_some(), "nothing to copy should be reported");
        assert!(copied.borrow().is_empty());

        // 클립보드 호출이 실패하면 알림 대신 오류가 나온다
        app.status = None;
        app.copier = Box::new(|_| Err(io::Error::other("no clipboard")));
        app.copy_current();
        assert!(app.status.as_deref().unwrap().contains("복사하지 못했습니다"));
        assert!(app.notice().is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    /// Finder를 띄우지 않도록 `open` 호출을 기록용으로 바꾼 앱
    fn app_with_recording_opener(
        dir: &Path,
    ) -> (App, std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>) {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut app = App::new(dir.to_path_buf());
        let sink = calls.clone();
        app.opener = Box::new(move |args| {
            sink.borrow_mut().push(args.iter().map(|a| a.to_string()).collect());
            Ok(())
        });
        wait(&mut app);
        (app, calls)
    }

    #[test]
    fn o_reveals_the_highlighted_item_and_capital_o_opens_the_current_folder() {
        let root = temp_root("finder");
        fs::write(root.join("내 큰 파일.bin"), vec![1u8; 300_000]).unwrap();
        fs::create_dir_all(root.join("sub dir")).unwrap();
        fs::write(root.join("sub dir/inner.txt"), vec![1u8; 10_000]).unwrap();

        let (mut app, calls) = app_with_recording_opener(&root);
        app.reveal_selected();
        assert_eq!(
            calls.borrow()[0],
            ["-R".to_string(), root.join("내 큰 파일.bin").to_string_lossy().into_owned()]
        );
        let notice = app.notice().unwrap().to_string();
        assert!(notice.contains("Finder에서 보기"), "{notice}");
        assert!(!notice.contains("숨김"), "a normal file needs no hidden-item hint: {notice}");

        app.open_current();
        assert_eq!(calls.borrow()[1], [root.to_string_lossy().into_owned()]);

        // 폴더 안에서는 그 폴더가 열리고, 항목은 전체 경로로 전달된다
        app.state.select(Some(1));
        app.enter();
        app.open_current();
        app.reveal_selected();
        assert_eq!(calls.borrow()[2], [root.join("sub dir").to_string_lossy().into_owned()]);
        assert_eq!(
            calls.borrow()[3],
            ["-R".to_string(), root.join("sub dir/inner.txt").to_string_lossy().into_owned()]
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn revealing_a_hidden_item_tells_how_to_see_it_in_finder() {
        let root = temp_root("finder-hidden");
        fs::create_dir_all(root.join(".cache")).unwrap();
        fs::write(root.join(".cache/blob"), vec![1u8; 300_000]).unwrap();
        fs::write(root.join("visible.txt"), vec![1u8; 1_000]).unwrap();

        let (mut app, calls) = app_with_recording_opener(&root);
        assert_eq!(names(&app)[0], ".cache");
        app.reveal_selected();
        assert_eq!(calls.borrow()[0][0], "-R");
        let notice = app.notice().unwrap().to_string();
        assert!(notice.contains("숨김 항목") && notice.contains("⌘⇧."), "{notice}");

        // 숨김 폴더 안의 일반 이름은 힌트가 필요 없다
        app.enter();
        app.reveal_selected();
        assert!(!app.notice().unwrap().contains("숨김"));

        // 폴더 자체를 여는 것은 숨김 힌트와 무관하다
        app.open_current();
        assert!(!app.notice().unwrap().contains("숨김"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finder_errors_and_empty_folders_are_reported() {
        let root = temp_root("finder-err");
        fs::create_dir_all(root.join("empty")).unwrap();

        let (mut app, calls) = app_with_recording_opener(&root);
        app.enter(); // 빈 폴더
        app.reveal_selected();
        assert_eq!(app.status.as_deref(), Some("열 항목이 없습니다"));
        assert!(calls.borrow().is_empty());

        app.status = None;
        app.opener = Box::new(|_| Err(io::Error::other("The file does not exist.")));
        app.open_current();
        let status = app.status.as_deref().unwrap();
        assert!(status.contains("Finder를 열지 못했습니다") && status.contains("does not exist"));
        assert!(app.notice().is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn u_and_s_change_the_unit_and_the_base_independently() {
        use crate::units::{Base, Unit};
        let root = temp_root("units");
        fs::write(root.join("f"), vec![1u8; 5_000]).unwrap();
        let mut app = App::new(root.clone());
        wait(&mut app);

        assert_eq!((app.units.base, app.units.unit), (Base::Binary, Unit::Auto));
        for want in [Unit::K, Unit::M, Unit::G, Unit::T, Unit::Auto] {
            app.cycle_unit();
            assert_eq!(app.units.unit, want);
        }
        app.cycle_unit(); // K
        app.toggle_base();
        assert_eq!((app.units.base, app.units.unit), (Base::Decimal, Unit::K));
        // 폴더를 옮겨도 표시 설정은 그대로다
        app.back();
        assert_eq!((app.units.base, app.units.unit), (Base::Decimal, Unit::K));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn i_shows_the_exact_size_and_marks_folders_that_could_not_be_fully_read() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("exact");
        fs::write(root.join("big.bin"), vec![1u8; 300_000]).unwrap();
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("locked/hidden"), vec![1u8; 10]).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let restricted = fs::read_dir(root.join("locked")).is_err();

        let mut app = App::new(root.clone());
        wait(&mut app);

        // 정확한 바이트와 두 기준의 값이 나란히 나온다 (단위 설정과 무관하게)
        app.cycle_unit(); // 고정 단위여도 i는 영향을 받지 않는다
        app.state.select(Some(0));
        let size = app.children()[0].size;
        assert_eq!(&*app.children()[0].name, "big.bin");
        app.show_exact();
        assert_eq!(app.notice(), Some(format!("크기: {}", units::exact(size)).as_str()));
        assert!(app.notice().unwrap().contains(" B ("), "{:?}", app.notice());

        if restricted {
            let idx = names(&app).iter().position(|n| n == "locked").unwrap();
            app.state.select(Some(idx));
            app.show_exact();
            assert!(app.notice().unwrap().starts_with("크기: ≥ "), "{:?}", app.notice());
        }

        // 항목이 없으면 알린다
        app.state.select(None);
        app.show_exact();
        assert_eq!(app.status.as_deref(), Some("표시할 항목이 없습니다"));

        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn expand_candidates_run_from_the_parent_up_to_the_filesystem_root() {
        let got = expand_candidates_for(Path::new("/Users/me/work"));
        assert_eq!(
            got,
            [
                PathBuf::from("/Users/me"),
                PathBuf::from("/Users"),
                PathBuf::from("/")
            ]
        );
        // 이미 최상위면 넓힐 곳이 없다 (디스크 전체를 스캔하지 않고 순수 함수로 확인한다)
        assert!(expand_candidates_for(Path::new("/")).is_empty());
    }

    #[test]
    fn prescanning_a_root_lets_you_start_deep_and_go_up_without_scanning() {
        let root = temp_root("prescan");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("z")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("a/g"), vec![1u8; 1_000]).unwrap();
        fs::write(root.join("z/big"), vec![1u8; 300_000]).unwrap();

        let start = root.join("a/b");
        let mut app = App::with_root(start.clone(), root.clone());
        assert_eq!(app.scanning.as_deref(), Some(root.as_path()), "the whole root is scanned");
        wait(&mut app);

        // 화면은 시작 폴더에 있고, 선택은 첫 항목
        assert_eq!(app.current, start);
        assert_eq!(names(&app), ["f"]);
        assert_eq!(app.state.selected(), Some(0));

        // 시작 폴더보다 위로, 루트까지 올라가도 스캔하지 않고 형제 폴더(z)도 이미 알고 있다
        app.back();
        assert_eq!(app.current, root.join("a"));
        app.back();
        assert!(app.scanning.is_none(), "everything under the root is already cached");
        assert_eq!(app.current, root);
        assert_eq!(names(&app), ["z", "a"]);
        assert_eq!(&*app.children()[app.state.selected().unwrap()].name, "a");

        // 형제 폴더로 들어가는 것도 즉시
        app.state.select(Some(0));
        app.enter();
        assert!(app.scanning.is_none());
        assert_eq!(names(&app), ["big"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_start_folder_outside_the_root_falls_back_to_the_root() {
        let root = temp_root("outside");
        let other = temp_root("outside-other");
        fs::write(root.join("f"), vec![1u8; 1_000]).unwrap();

        let mut app = App::with_root(other.clone(), root.clone());
        wait(&mut app);
        assert_eq!(app.current, root);
        assert_eq!(names(&app), ["f"]);

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn a_refresh_reports_what_grew_what_is_new_and_what_stayed() {
        let root = temp_root("changes");
        fs::write(root.join("grows"), vec![1u8; 100_000]).unwrap();
        fs::write(root.join("same"), vec![1u8; 10_000]).unwrap();

        let mut app = App::new(root.clone());
        wait(&mut app);
        assert!(app.change_of("grows").is_none(), "nothing to compare on the first scan");
        assert!(app.last_change.is_none());

        fs::write(root.join("grows"), vec![1u8; 400_000]).unwrap();
        fs::write(root.join("fresh"), vec![1u8; 50_000]).unwrap();
        app.rescan();
        wait(&mut app);

        let grows = app.change_of("grows").expect("grows should be flagged");
        assert!(!grows.is_new);
        assert!(grows.delta >= 250_000, "delta was {}", grows.delta);

        let fresh = app.change_of("fresh").expect("fresh should be flagged");
        assert!(fresh.is_new);
        assert!(fresh.delta >= 50_000);

        assert!(app.change_of("same").is_none(), "unchanged entries are not flagged");

        let (_, total) = app.last_change.expect("the total changed");
        assert_eq!(total, grows.delta + fresh.delta);

        // 한 번 더 갱신해도 변화가 없으면 기존 표시는 그대로 남고 확인 시각만 갱신된다
        let checked = app.last_checked;
        app.rescan();
        wait(&mut app);
        assert!(app.change_of("grows").is_some());
        assert!(app.last_checked > checked);

        // 다른 폴더를 보고 있을 때는 변화 표시가 보이지 않는다
        fs::create_dir_all(root.join("sub")).unwrap();
        app.rescan();
        wait(&mut app);
        let sub = names(&app).iter().position(|n| n == "sub").unwrap();
        app.state.select(Some(sub));
        app.enter();
        assert!(app.change_of("grows").is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn watch_intervals_step_with_a_floor_and_no_ceiling() {
        assert_eq!(step_up(2), 3);
        assert_eq!(step_up(5), 10);
        assert_eq!(step_up(3600), 7200);
        assert_eq!(step_up(7200), 14400);
        assert_eq!(step_down(10), 5);
        assert_eq!(step_down(3), 2);
        assert_eq!(step_down(2), MIN_WATCH_SECS); // 더 내려가지 않는다
        assert_eq!(step_down(7200), 3600);
    }

    #[test]
    fn watch_never_overlaps_scans_and_slows_down_for_big_folders() {
        let root = temp_root("watch");
        fs::write(root.join("f"), vec![1u8; 1_000]).unwrap();
        let mut app = App::new(root.clone());
        wait(&mut app);

        assert!(!app.watch_due(Instant::now()), "watch is off by default");

        app.enable_watch(1); // 최솟값으로 올라간다
        assert_eq!(app.watch.as_ref().unwrap().secs, MIN_WATCH_SECS);

        let now = Instant::now();
        assert!(!app.watch_due(now), "not due right after being scheduled");
        assert!(app.watch_due(now + Duration::from_secs(3)));

        // 스캔 중에는 시각이 지났어도 시작하지 않는다
        app.rescan();
        assert!(!app.watch_due(now + Duration::from_secs(60)));
        wait(&mut app);

        // 간격 < 마지막 스캔 시간 × 2 이면 실제 간격이 늘어난다
        app.last_scan = Some(Duration::from_secs(10));
        assert_eq!(app.watch_effective(), Some(Duration::from_secs(20)));
        app.last_scan = Some(Duration::from_millis(100));
        assert_eq!(app.watch_effective(), Some(Duration::from_secs(MIN_WATCH_SECS)));

        // 간격 조절과 껐다 켜기
        app.enable_watch(5);
        app.adjust_watch(true);
        assert_eq!(app.watch.as_ref().unwrap().secs, 10);
        app.adjust_watch(false);
        app.adjust_watch(false);
        assert_eq!(app.watch.as_ref().unwrap().secs, 3);
        app.toggle_watch();
        assert!(app.watch.is_none());
        app.toggle_watch(); // 마지막에 쓴 간격을 기억한다
        assert_eq!(app.watch.as_ref().unwrap().secs, 3);

        fs::remove_dir_all(&root).unwrap();
    }
}

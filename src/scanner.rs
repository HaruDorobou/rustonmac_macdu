use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;

/// 스캔 결과 트리의 노드 하나 (파일 또는 폴더).
///
/// 폴더를 한 번 훑으면 하위 전체의 크기를 이미 계산하게 되므로, 그 결과를 버리지 않고
/// 트리로 들고 있다. 그러면 폴더를 오가는 일이 다시 스캔 없이 즉시 된다.
/// 메모리를 아끼려고 이름은 `Box<str>`, 자식은 `Box<[Node]>`로 들고 있다.
#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    pub is_dir: bool,
    /// 디스크 실제 사용량 (바이트). 논리 크기가 아니라 할당된 블록 기준.
    /// `unreadable > 0`이면 읽지 못한 부분이 빠져 있으므로 실제보다 작은 값이다.
    pub size: u64,
    /// 이 노드 안(폴더면 하위 전체, 노드 자체를 못 읽었으면 자기 자신 포함)에서
    /// 읽지 못한 경로의 수
    pub unreadable: u32,
    /// 폴더면 하위 항목들 (크기 내림차순). 파일이면 비어 있다.
    pub children: Box<[Node]>,
}

impl Node {
    pub fn empty() -> Self {
        Node {
            name: "".into(),
            is_dir: true,
            size: 0,
            unreadable: 0,
            children: Box::new([]),
        }
    }

    /// `path`(루트 아래 폴더 이름들)가 가리키는 폴더 노드. 없으면 `None`.
    pub fn find(&self, path: &[String]) -> Option<&Node> {
        let mut node = self;
        for name in path {
            node = node.children.iter().find(|c| c.is_dir && *c.name == **name)?;
        }
        Some(node)
    }

    /// `path`(루트 아래 폴더 이름들)가 가리키는 폴더의 내용을 `new`로 바꾸고,
    /// 지나온 상위 폴더들의 크기와 정렬을 다시 계산한다. 경로가 없으면 `false`.
    pub fn replace_at(&mut self, path: &[String], new: Node) -> bool {
        let Some((first, rest)) = path.split_first() else {
            let name = std::mem::take(&mut self.name);
            *self = Node { name, ..new };
            return true;
        };
        let Some(idx) = self
            .children
            .iter()
            .position(|c| c.is_dir && *c.name == **first)
        else {
            return false;
        };
        if !self.children[idx].replace_at(rest, new) {
            return false;
        }
        self.size = self.children.iter().map(|c| c.size).sum();
        self.unreadable = self.children.iter().map(|c| c.unreadable).sum();
        self.children.sort_by(|a, b| b.size.cmp(&a.size));
        true
    }
}

/// 스캔 중 읽지 못한 경로 하나
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: PathBuf,
    /// OS 오류 번호 (`errno`). 알 수 없으면 0.
    pub errno: i32,
}

/// 스캔 중 발생한 오류 모음
#[derive(Debug, Default)]
pub struct ErrorReport {
    /// 기록된 오류 (최대 `MAX_REPORTED`건)
    pub list: Vec<ScanError>,
    /// 기록 한도와 관계없는 전체 오류 건수
    pub total: usize,
}

/// 오류가 아주 많아도 메모리를 아끼기 위해 기록하는 건수에 상한을 둔다.
const MAX_REPORTED: usize = 2000;

impl ErrorReport {
    /// `dir` 아래 경로의 오류 기록을 지우고, 다시 스캔한 결과 `fresh`로 바꾼다.
    pub fn replace_under(&mut self, dir: &Path, removed_count: usize, fresh: ErrorReport) {
        self.list.retain(|e| !e.path.starts_with(dir));
        self.total = self.total.saturating_sub(removed_count) + fresh.total;
        let room = MAX_REPORTED.saturating_sub(self.list.len());
        self.list.extend(fresh.list.into_iter().take(room));
    }

    /// 다른 기록을 이 기록에 합친다.
    pub fn absorb(&mut self, other: ErrorReport) {
        self.total += other.total;
        let room = MAX_REPORTED.saturating_sub(self.list.len());
        self.list.extend(other.list.into_iter().take(room));
    }
}

pub struct ScanOutput {
    /// 스캔한 폴더 자체를 나타내는 노드 (`name`은 비어 있다)
    pub root: Node,
    pub errors: ErrorReport,
}

/// 병렬 스캔 중 여러 스레드가 함께 쓰는 오류 기록장. 오류는 드물어서 락 경합은 무시할 수 있다.
#[derive(Default)]
struct ErrorLog(Mutex<ErrorReport>);

impl ErrorLog {
    fn push(&self, path: PathBuf, errno: i32) {
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        report.total += 1;
        if report.list.len() < MAX_REPORTED {
            report.list.push(ScanError { path, errno });
        }
    }

    fn into_report(self) -> ErrorReport {
        self.0.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// 스캔에 쓰는 스레드 수의 상한.
/// 실측(16코어 기기): 16스레드까지는 스레드가 늘수록 빨라지고 안정적이었지만,
/// 20스레드 이상에서는 5~7초씩 멈추는 현상이 반복됐다.
const MAX_SCAN_THREADS: usize = 16;

fn scan_threads(cpus: usize) -> usize {
    cpus.clamp(1, MAX_SCAN_THREADS)
}

/// 스캔용 rayon 스레드 풀을 설정한다. 스캔을 시작하기 전에 한 번 호출해야 한다.
///
/// 우선순위: 명시한 `threads`(`--threads`) > `RAYON_NUM_THREADS` > 기본값(`min(코어, 16)`).
/// 명시한 값은 상한 없이 그대로 쓴다.
pub fn init_thread_pool(threads: Option<usize>) {
    let n = match threads {
        Some(n) => n,
        None if std::env::var_os("RAYON_NUM_THREADS").is_some() => return,
        None => scan_threads(std::thread::available_parallelism().map_or(4, |n| n.get())),
    };
    // 이미 전역 풀이 만들어져 있으면 실패하지만, 그대로 쓰면 되므로 무시한다
    let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
}

/// `root` 아래 전체를 병렬로 스캔해 트리로 반환한다. 각 폴더의 자식은 크기 내림차순이다.
/// `root` 자체를 열 수 없으면 `Err`. 그 아래에서 읽지 못한 경로는 `errors`에 모은다.
#[cfg(test)]
pub fn scan_tree(root: &Path) -> io::Result<ScanOutput> {
    scan_tree_skipping(root, None)
}

/// `scan_tree`와 같지만 `skip` 폴더는 읽지 않고 비어 있는 자리표시 노드로 남긴다.
/// 이미 스캔해 둔 하위 트리를 `Node::replace_at`으로 끼워 넣을 때 쓴다.
pub fn scan_tree_skipping(root: &Path, skip: Option<&Path>) -> io::Result<ScanOutput> {
    let log = ErrorLog::default();
    let raws = read_dir_bulk(root, &log)?;
    let root = build_dir_node("".into(), root, raws, &log, skip);
    Ok(ScanOutput {
        root,
        errors: log.into_report(),
    })
}

/// 폴더를 열어서 노드로 만든다. 열지 못하면 오류를 기록하고 비어 있는 노드를 돌려준다.
fn scan_dir_node(name: Box<str>, path: &Path, log: &ErrorLog, skip: Option<&Path>) -> Node {
    match read_dir_bulk(path, log) {
        Ok(raws) => build_dir_node(name, path, raws, log, skip),
        Err(e) => {
            log.push(path.to_path_buf(), e.raw_os_error().unwrap_or(0));
            Node {
                name,
                is_dir: true,
                size: 0,
                unreadable: 1,
                children: Box::new([]),
            }
        }
    }
}

/// 이미 읽은 항목 목록으로 폴더 노드를 만든다. 하위 폴더는 병렬로 재귀한다.
/// 심볼릭 링크는 따라가지 않는다 (순환 방지).
fn build_dir_node(
    name: Box<str>,
    path: &Path,
    raws: Vec<Raw>,
    log: &ErrorLog,
    skip: Option<&Path>,
) -> Node {
    // 파일은 값싸게 바로 처리하고, 하위 폴더만 병렬로 재귀한다
    let (dirs, others): (Vec<Raw>, Vec<Raw>) = raws
        .into_iter()
        .partition(|r| r.kind == Kind::Dir && !r.failed);

    let mut children: Vec<Node> = others
        .into_iter()
        .map(|r| Node {
            is_dir: r.kind == Kind::Dir,
            size: r.alloc,
            unreadable: u32::from(r.failed),
            name: r.name.into_boxed_str(),
            children: Box::new([]),
        })
        .collect();

    children.par_extend(dirs.into_par_iter().map(|r| {
        let child_path = path.join(&r.name);
        let name = r.name.into_boxed_str();
        if skip == Some(child_path.as_path()) {
            // 이미 스캔해 둔 폴더. 읽지 않고 자리만 잡아 두면 나중에 끼워 넣는다.
            return Node {
                name,
                is_dir: true,
                size: 0,
                unreadable: 0,
                children: Box::new([]),
            };
        }
        scan_dir_node(name, &child_path, log, skip)
    }));

    children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    Node {
        name,
        is_dir: true,
        size: children.iter().map(|c| c.size).sum(),
        unreadable: children.iter().map(|c| c.unreadable).sum(),
        children: children.into_boxed_slice(),
    }
}

// ---------------------------------------------------------------------------
// getattrlistbulk(2): 디렉토리의 모든 항목의 이름/종류/할당 크기를
// 시스템 콜 한 번으로 읽는다. 항목마다 lstat을 부르는 것보다 호출 수가 훨씬 적다.
// ---------------------------------------------------------------------------

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: c_int,
        alist: *mut AttrList,
        buffer: *mut c_void,
        buffer_size: usize,
        options: u64,
    ) -> c_int;
}

// <sys/attr.h>
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;

// <sys/vnode.h> enum vtype
const VREG: u32 = 1;
const VDIR: u32 = 2;

const BUF_SIZE: usize = 64 * 1024;

thread_local! {
    /// 디렉토리마다 할당하지 않도록 스레드별로 재사용하는 버퍼
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; BUF_SIZE]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Other,
}

struct Raw {
    name: String,
    kind: Kind,
    /// 할당된 바이트 수 (파일일 때만 의미 있음)
    alloc: u64,
    /// 이 항목의 속성을 읽지 못했다 (크기 0으로 취급)
    failed: bool,
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
}

fn i32_at(buf: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap())
}

/// `path`의 직계 자식을 읽는다. 폴더 자체를 열지 못하면 `Err`.
/// 항목 하나만 읽지 못한 경우(`ATTR_CMN_ERROR`)는 `log`에 기록하고 계속한다.
fn read_dir_bulk(path: &Path, log: &ErrorLog) -> io::Result<Vec<Raw>> {
    let dir = File::open(path)?;
    let fd = dir.as_raw_fd();

    let mut list = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_ERROR,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };

    let mut out = Vec::new();

    BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        loop {
            // SAFETY: buf는 BUF_SIZE 바이트의 유효한 쓰기 가능 메모리이고,
            // list는 위에서 초기화한 유효한 attrlist다.
            let n = unsafe {
                getattrlistbulk(fd, &mut list, buf.as_mut_ptr().cast(), buf.len(), 0)
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(());
            }

            let mut off = 0usize;
            for _ in 0..n {
                let len = u32_at(&buf, off) as usize;
                match parse_entry(&buf[off..off + len]) {
                    Some(Parsed::Entry(raw)) => out.push(raw),
                    Some(Parsed::Error { name, errno, kind }) => {
                        log.push(path.join(&name), errno);
                        // 목록에서 사라지지 않도록 크기 0의 항목으로 남긴다
                        out.push(Raw {
                            name,
                            kind,
                            alloc: 0,
                            failed: true,
                        });
                    }
                    None => {}
                }
                off += len;
            }
        }
    })?;

    Ok(out)
}

enum Parsed {
    Entry(Raw),
    /// 이 항목의 속성을 읽지 못함 (권한 등)
    Error {
        name: String,
        errno: i32,
        /// 오류가 나도 종류(OBJTYPE)는 대개 알 수 있다
        kind: Kind,
    },
}

/// 버퍼 안의 항목 하나를 파싱한다. 속성은 요청한 순서(공통 → 파일)대로,
/// "returned attrs"에 실제로 담긴 것만 이어서 들어 있다.
fn parse_entry(e: &[u8]) -> Option<Parsed> {
    // [0..4] 길이, [4..24] 반환된 속성 집합(attribute_set_t: 5 x u32)
    let ret_common = u32_at(e, 4);
    let ret_file = u32_at(e, 4 + 12);
    let mut p = 4 + 20;

    let mut name = None;
    if ret_common & ATTR_CMN_NAME != 0 {
        // attrreference_t: 이 필드 위치 기준 오프셋 + 길이(NUL 포함)
        let data_off = i32_at(e, p);
        let data_len = u32_at(e, p + 4) as usize;
        let start = (p as i64 + data_off as i64) as usize;
        let bytes = e.get(start..start + data_len.saturating_sub(1))?;
        name = Some(String::from_utf8_lossy(bytes).into_owned());
        p += 8;
    }

    let mut objtype = 0;
    if ret_common & ATTR_CMN_OBJTYPE != 0 {
        objtype = u32_at(e, p);
        p += 4;
    }

    let kind = match objtype {
        VDIR => Kind::Dir,
        VREG => Kind::File,
        _ => Kind::Other,
    };

    if ret_common & ATTR_CMN_ERROR != 0 {
        // OBJTYPE 바로 뒤에 errno(u32)가 온다. 크기는 알 수 없다.
        let errno = u32_at(e, p) as i32;
        return name.map(|name| Parsed::Error { name, errno, kind });
    }

    let mut alloc = 0;
    if ret_file & ATTR_FILE_ALLOCSIZE != 0 {
        alloc = u64_at(e, p);
    }

    Some(Parsed::Entry(Raw {
        name: name?,
        kind,
        alloc,
        failed: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mac-du-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
        node.children
            .iter()
            .find(|c| &*c.name == name)
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    #[test]
    fn scan_threads_is_capped() {
        assert_eq!(scan_threads(1), 1);
        assert_eq!(scan_threads(8), 8);
        assert_eq!(scan_threads(16), 16);
        assert_eq!(scan_threads(24), MAX_SCAN_THREADS);
        assert_eq!(scan_threads(128), MAX_SCAN_THREADS);
        assert_eq!(scan_threads(0), 1);
    }

    #[test]
    fn tree_keeps_every_level_with_consistent_sizes() {
        let root = temp_root("tree");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/small"), vec![1u8; 100]).unwrap();
        fs::write(root.join("a/b/big"), vec![1u8; 200_000]).unwrap();
        fs::write(root.join("top"), vec![1u8; 5_000]).unwrap();

        let out = scan_tree(&root).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let a = child(&out.root, "a");
        let b = child(a, "b");
        let big = child(b, "big");
        assert!(a.is_dir && b.is_dir && !big.is_dir);
        assert!(big.size >= 200_000);

        // 폴더 크기는 자식 크기의 합이다 (모든 단계에서)
        assert_eq!(b.size, b.children.iter().map(|c| c.size).sum::<u64>());
        assert_eq!(a.size, a.children.iter().map(|c| c.size).sum::<u64>());
        assert_eq!(out.root.size, out.root.children.iter().map(|c| c.size).sum::<u64>());
        assert!(a.size >= b.size);

        // 자식은 크기 내림차순
        for n in [&out.root, a, b] {
            assert!(n.children.windows(2).all(|w| w[0].size >= w[1].size));
        }
        assert_eq!(out.errors.total, 0);
    }

    #[test]
    fn replace_at_updates_ancestor_sizes_and_order() {
        let root = temp_root("graft");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/f"), vec![1u8; 50_000]).unwrap();
        fs::write(root.join("c/f"), vec![1u8; 100_000]).unwrap();

        let mut tree = scan_tree(&root).unwrap().root;
        // 처음에는 c(100k)가 a(50k)보다 크다
        assert_eq!(&*tree.children[0].name, "c");

        // a/b 안에 큰 파일을 추가하고, a/b만 다시 스캔해 끼워 넣는다
        fs::write(root.join("a/b/g"), vec![1u8; 400_000]).unwrap();
        let fresh = scan_tree(&root.join("a/b")).unwrap().root;
        let ok = tree.replace_at(&["a".to_string(), "b".to_string()], fresh);
        fs::remove_dir_all(&root).unwrap();

        assert!(ok);
        // a가 커져서 순서가 바뀌고, 루트 합계도 새 값과 맞아야 한다
        assert_eq!(&*tree.children[0].name, "a");
        let a = child(&tree, "a");
        let b = child(a, "b");
        assert!(b.size >= 450_000);
        assert_eq!(a.size, b.size);
        assert_eq!(tree.size, a.size + child(&tree, "c").size);
        // 없는 경로는 실패하고 트리를 바꾸지 않는다
        assert!(!tree.replace_at(&["nope".to_string()], Node::empty()));
    }

    #[test]
    fn skipping_a_subtree_leaves_a_placeholder_that_can_be_filled_in_later() {
        let root = temp_root("skip");
        fs::create_dir_all(root.join("known/deep")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("known/deep/f"), vec![1u8; 80_000]).unwrap();
        fs::write(root.join("other/f"), vec![1u8; 30_000]).unwrap();
        fs::write(root.join("top"), vec![1u8; 5_000]).unwrap();

        let full = scan_tree(&root).unwrap();
        let known = scan_tree(&root.join("known")).unwrap();
        let mut partial = scan_tree_skipping(&root, Some(&root.join("known"))).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // 건너뛴 폴더는 비어 있는 자리표시만 남는다
        let placeholder = child(&partial.root, "known");
        assert_eq!(placeholder.size, 0);
        assert!(placeholder.children.is_empty());
        assert!(partial.root.size < full.root.size);

        // 미리 스캔해 둔 하위 트리를 끼워 넣으면 전체를 한 번에 스캔한 것과 같아진다
        assert!(partial.root.replace_at(&["known".to_string()], known.root));
        assert_eq!(partial.root.size, full.root.size);
        let names = |n: &Node| n.children.iter().map(|c| c.name.to_string()).collect::<Vec<_>>();
        assert_eq!(names(&partial.root), names(&full.root));
        assert_eq!(
            partial.root.find(&["known".into(), "deep".into()]).unwrap().size,
            full.root.find(&["known".into(), "deep".into()]).unwrap().size
        );
        assert!(partial.root.find(&["nope".into()]).is_none());
    }

    #[test]
    fn unreadable_dir_is_reported_not_silently_dropped() {
        let root = temp_root("locked");
        fs::create_dir_all(root.join("ok")).unwrap();
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("ok/file"), vec![1u8; 10_000]).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

        // root 권한으로 돌리면 접근 제한이 적용되지 않아 검증할 수 없다
        let restricted = fs::read_dir(root.join("locked")).is_err();

        let out = scan_tree(&root).unwrap();

        // 지우려면 권한을 되돌려야 한다. 단언이 실패해도 임시 폴더가 남지 않게 먼저 정리한다.
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        if !restricted {
            return;
        }
        assert_eq!(out.errors.total, 1);
        assert!(out.errors.list[0].path.ends_with("locked"));
        assert_eq!(out.errors.list[0].errno, 13); // EACCES

        let locked = child(&out.root, "locked");
        assert_eq!(locked.size, 0);
        assert_eq!(locked.unreadable, 1);
        let ok = child(&out.root, "ok");
        assert!(ok.size >= 10_000);
        assert_eq!(ok.unreadable, 0);
        assert_eq!(out.root.unreadable, 1);
    }

    #[test]
    fn nested_unreadable_marks_every_ancestor() {
        let root = temp_root("nested");
        let inner = root.join("outer/mid/locked");
        fs::create_dir_all(&inner).unwrap();
        fs::write(root.join("outer/data"), vec![1u8; 10_000]).unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o000)).unwrap();

        let restricted = fs::read_dir(&inner).is_err();
        let out = scan_tree(&root).unwrap();

        fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        if !restricted {
            return;
        }
        let outer = child(&out.root, "outer");
        assert_eq!(outer.unreadable, 1);
        assert_eq!(child(outer, "mid").unreadable, 1);
        assert!(outer.size >= 10_000);
        assert_eq!(out.root.unreadable, 1);
        assert_eq!(out.errors.total, 1);
    }
}

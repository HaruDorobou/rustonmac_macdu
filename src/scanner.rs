use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// 디스크 실제 사용량 (바이트). 논리 크기가 아니라 할당된 블록 기준.
    /// `unreadable > 0`이면 읽지 못한 부분이 빠져 있으므로 실제보다 작은 값이다.
    pub size: u64,
    /// 이 항목 안(폴더면 하위 전체, 항목 자체를 못 읽었으면 자기 자신 포함)에서
    /// 읽지 못한 경로의 수
    pub unreadable: usize,
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

impl ErrorReport {
    fn merge(&mut self, other: ErrorReport) {
        self.total += other.total;
        let room = MAX_REPORTED.saturating_sub(self.list.len());
        self.list.extend(other.list.into_iter().take(room));
    }
}

/// 오류가 아주 많아도 메모리를 아끼기 위해 기록하는 건수에 상한을 둔다.
const MAX_REPORTED: usize = 2000;

pub struct ScanOutput {
    pub entries: Vec<Entry>,
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

/// `root`의 직계 자식들을 병렬로 스캔해 용량 내림차순으로 반환한다.
/// `root` 자체를 열 수 없으면 `Err`. 그 아래에서 읽지 못한 경로는 `errors`에 모은다.
pub fn scan_dir(root: &Path) -> io::Result<ScanOutput> {
    let root_log = ErrorLog::default();
    let raws = read_dir_bulk(root, &root_log)?;

    // 항목마다 자기 기록장을 써서 "이 항목 안에서 읽지 못한 건수"를 따로 센다
    let results: Vec<(Entry, ErrorReport)> = raws
        .par_iter()
        .map(|raw| {
            let path = root.join(&raw.name);
            let is_dir = raw.kind == Kind::Dir;
            let log = ErrorLog::default();
            // 항목 자체를 못 읽은 폴더는 들어가 봐야 같은 오류만 반복되므로 건너뛴다
            let size = if is_dir && !raw.failed {
                dir_size(&path, &log)
            } else {
                raw.alloc
            };
            let report = log.into_report();
            let entry = Entry {
                name: raw.name.clone(),
                path,
                is_dir,
                size,
                unreadable: report.total + usize::from(raw.failed),
            };
            (entry, report)
        })
        .collect();

    // 항목 자체를 못 읽은 경우는 root_log에 이미 들어 있다
    let mut errors = root_log.into_report();
    let mut entries = Vec::with_capacity(results.len());
    for (entry, report) in results {
        errors.merge(report);
        entries.push(entry);
    }

    entries.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(ScanOutput { entries, errors })
}

/// 디렉토리를 재귀적으로(병렬로) 합산한다.
/// 심볼릭 링크는 따라가지 않는다 (순환 방지).
/// 읽지 못한 곳은 0으로 취급하되 `log`에 기록한다.
fn dir_size(path: &Path, log: &ErrorLog) -> u64 {
    let raws = match read_dir_bulk(path, log) {
        Ok(raws) => raws,
        Err(e) => {
            log.push(path.to_path_buf(), e.raw_os_error().unwrap_or(0));
            return 0;
        }
    };

    let files: u64 = raws
        .iter()
        .filter(|r| r.kind != Kind::Dir)
        .map(|r| r.alloc)
        .sum();
    let subdirs: u64 = raws
        .par_iter()
        .filter(|r| r.kind == Kind::Dir && !r.failed)
        .map(|r| dir_size(&path.join(&r.name), log))
        .sum();

    files + subdirs
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
    fn unreadable_dir_is_reported_not_silently_dropped() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("mac-du-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ok")).unwrap();
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("ok/file"), vec![1u8; 10_000]).unwrap();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

        // root 권한으로 돌리면 접근 제한이 적용되지 않아 검증할 수 없다
        let restricted = fs::read_dir(root.join("locked")).is_err();

        let out = scan_dir(&root).unwrap();

        // 지우려면 권한을 되돌려야 한다. 단언이 실패해도 임시 폴더가 남지 않게 먼저 정리한다.
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        if !restricted {
            return;
        }
        assert_eq!(out.errors.total, 1);
        assert!(out.errors.list[0].path.ends_with("locked"));
        assert_eq!(out.errors.list[0].errno, 13); // EACCES

        let locked = out.entries.iter().find(|e| e.name == "locked").unwrap();
        assert_eq!(locked.size, 0);
        assert_eq!(locked.unreadable, 1);
        let ok = out.entries.iter().find(|e| e.name == "ok").unwrap();
        assert!(ok.size >= 10_000);
        assert_eq!(ok.unreadable, 0);
    }

    #[test]
    fn nested_unreadable_marks_every_ancestor_entry() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("mac-du-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let inner = root.join("outer/mid/locked");
        fs::create_dir_all(&inner).unwrap();
        fs::write(root.join("outer/data"), vec![1u8; 10_000]).unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o000)).unwrap();

        let restricted = fs::read_dir(&inner).is_err();
        let out = scan_dir(&root).unwrap();

        fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        if !restricted {
            return;
        }
        // scan_dir(root)의 직계 자식은 outer 하나다. 안쪽 깊은 곳의 오류가 outer에 집계돼야 한다.
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].name, "outer");
        assert_eq!(out.entries[0].unreadable, 1);
        assert!(out.entries[0].size >= 10_000);
        assert_eq!(out.errors.total, 1);
    }
}

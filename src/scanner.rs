use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// 디스크 실제 사용량 (바이트). 논리 크기가 아니라 할당된 블록 기준.
    pub size: u64,
}

/// 스캔에 쓰는 스레드 수의 상한.
/// 실측(16코어 기기): 16스레드까지는 스레드가 늘수록 빨라지고 안정적이었지만,
/// 20스레드 이상에서는 5~7초씩 멈추는 현상이 반복됐다.
const MAX_SCAN_THREADS: usize = 16;

fn scan_threads(cpus: usize) -> usize {
    cpus.clamp(1, MAX_SCAN_THREADS)
}

/// 스캔용 rayon 스레드 풀을 설정한다. 스캔을 시작하기 전에 한 번 호출해야 한다.
/// 실험용으로 `RAYON_NUM_THREADS`가 지정돼 있으면 그 값을 존중한다.
pub fn init_thread_pool() {
    if std::env::var_os("RAYON_NUM_THREADS").is_some() {
        return;
    }
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    // 이미 전역 풀이 만들어져 있으면 실패하지만, 그대로 쓰면 되므로 무시한다
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(scan_threads(cpus))
        .build_global();
}

/// `root`의 직계 자식들을 병렬로 스캔해 용량 내림차순으로 반환한다.
pub fn scan_dir(root: &Path) -> io::Result<Vec<Entry>> {
    let raws = read_dir_bulk(root)?;

    let mut entries: Vec<Entry> = raws
        .par_iter()
        .map(|raw| {
            let path = root.join(&raw.name);
            let is_dir = raw.kind == Kind::Dir;
            let size = if is_dir { dir_size(&path) } else { raw.alloc };
            Entry {
                name: raw.name.clone(),
                path,
                is_dir,
                size,
            }
        })
        .collect();

    entries.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(entries)
}

/// 디렉토리를 재귀적으로(병렬로) 합산한다.
/// 심볼릭 링크는 따라가지 않는다 (순환 방지). 권한 없는 곳은 0으로 취급한다.
fn dir_size(path: &Path) -> u64 {
    let Ok(raws) = read_dir_bulk(path) else { return 0 };

    let files: u64 = raws
        .iter()
        .filter(|r| r.kind != Kind::Dir)
        .map(|r| r.alloc)
        .sum();
    let subdirs: u64 = raws
        .par_iter()
        .filter(|r| r.kind == Kind::Dir)
        .map(|r| dir_size(&path.join(&r.name)))
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

fn read_dir_bulk(path: &Path) -> io::Result<Vec<Raw>> {
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
                if let Some(raw) = parse_entry(&buf[off..off + len]) {
                    out.push(raw);
                }
                off += len;
            }
        }
    })?;

    Ok(out)
}

/// 버퍼 안의 항목 하나를 파싱한다. 속성은 요청한 순서(공통 → 파일)대로,
/// "returned attrs"에 실제로 담긴 것만 이어서 들어 있다.
fn parse_entry(e: &[u8]) -> Option<Raw> {
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

    if ret_common & ATTR_CMN_ERROR != 0 {
        // 이 항목의 속성을 읽지 못함 (권한 등). 건너뛴다.
        return None;
    }

    let mut alloc = 0;
    if ret_file & ATTR_FILE_ALLOCSIZE != 0 {
        alloc = u64_at(e, p);
    }

    let kind = match objtype {
        VDIR => Kind::Dir,
        VREG => Kind::File,
        _ => Kind::Other,
    };

    Some(Raw {
        name: name?,
        kind,
        alloc,
    })
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
}

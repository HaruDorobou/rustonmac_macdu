use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    /// 디스크 실제 사용량 (바이트). 논리 크기가 아니라 할당된 블록 기준.
    pub size: u64,
}

/// `root`의 직계 자식들을 스캔해 용량 내림차순으로 반환한다.
pub fn scan_dir(root: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();

    for dirent in fs::read_dir(root)? {
        let Ok(dirent) = dirent else { continue };
        let path = dirent.path();
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };

        let is_dir = meta.is_dir();
        let size = if is_dir { dir_size(&path) } else { disk_usage(&meta) };

        entries.push(Entry {
            name: dirent.file_name().to_string_lossy().into_owned(),
            is_dir,
            size,
        });
    }

    entries.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(entries)
}

/// 디렉토리를 재귀적으로 합산한다.
/// 심볼릭 링크는 따라가지 않는다 (순환 방지). 권한 없는 곳은 0으로 취급한다.
fn dir_size(path: &Path) -> u64 {
    let Ok(read) = fs::read_dir(path) else { return 0 };

    read.filter_map(Result::ok)
        .map(|e| {
            let child = e.path();
            match fs::symlink_metadata(&child) {
                Ok(m) if m.is_dir() => dir_size(&child),
                Ok(m) => disk_usage(&m),
                Err(_) => 0,
            }
        })
        .sum()
}

/// `st_blocks`는 512바이트 단위다. Finder의 "디스크 사용량"에 가까운 값이 된다.
fn disk_usage(meta: &fs::Metadata) -> u64 {
    meta.blocks() * 512
}

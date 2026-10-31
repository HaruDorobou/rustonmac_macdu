use std::io;
use std::process::{Command, Stdio};

/// `open`을 주어진 인자로 실행한다 (`open -R 경로`는 Finder에서 그 항목을 선택해 보여 주고,
/// `open 경로`는 폴더를 연다).
///
/// 출력은 파이프로 받아 TUI 화면을 어지럽히지 않는다. 실패하면 `open`이 낸 메시지를 오류에 담는다.
pub fn open(args: &[&str]) -> io::Result<()> {
    let out = Command::new("open")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(io::Error::other(if msg.is_empty() {
        format!("open이 {}로 끝났습니다", out.status)
    } else {
        msg
    }))
}

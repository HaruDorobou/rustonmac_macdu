use std::io::{self, Write};
use std::process::{Command, Stdio};

/// macOS 클립보드에 `text`를 넣는다 (`pbcopy`).
///
/// 텍스트는 표준 입력으로 넘기므로 공백이나 한글이 들어 있어도 이스케이프가 필요 없다.
/// `pbcopy`의 출력이 TUI 화면을 어지럽히지 않도록 표준 출력과 오류는 버린다.
pub fn copy(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // 쓰고 나서 stdin을 닫아야 pbcopy가 입력이 끝난 것을 알고 종료한다
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("pbcopy가 {status}로 끝났습니다")))
    }
}

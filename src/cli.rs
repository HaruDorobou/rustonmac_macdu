use std::path::PathBuf;

use crate::app::MIN_WATCH_SECS;

pub const HELP: &str = "\
mac-du - macOS 디스크 사용량 TUI

사용법:
    mac-du [옵션] [경로]

경로:
    스캔할 폴더. 생략하면 현재 폴더입니다.

옵션:
    -j, --threads <N>    스캔에 쓸 스레드 수 (1 이상).
                         기본값은 min(CPU 코어 수, 16)입니다.
                         코어 수보다 크게 지정하면 느려집니다. 16코어 기기에서는
                         20스레드부터 스캔이 5~7초씩 멈췄습니다.
    -r, --root <DIR>     DIR 전체를 먼저 스캔해 두고, 화면은 시작 폴더(경로 인자 또는
                         현재 폴더)에서 시작합니다. DIR 아래에서는 어느 폴더로 오가도
                         다시 스캔하지 않습니다. DIR이 넓을수록 첫 스캔 시간과 메모리
                         사용량이 늘어납니다. 시작 폴더가 DIR 밖이면 DIR에서 시작합니다.
    -w, --watch <SEC>    시작할 때부터 감시 모드를 켜고 SEC초마다 현재 폴더를 다시 스캔
                         (2 이상, 상한 없음). 마지막 스캔에 걸린 시간의 2배보다 짧은
                         간격으로는 돌지 않습니다. 앱 안에서는 w 키로 켜고 끄고,
                         +/- 키로 간격을 바꿉니다.
    --si                 크기를 1000 기준(KB, MB, GB. Finder와 같음)으로 표시합니다.
                         기본은 1024 기준(KiB, MiB, GiB. du -h와 같음)입니다.
                         앱 안에서는 s 키로 기준을, u 키로 단위(자동/K/M/G/T)를 바꿉니다.
    -h, --help           이 도움말을 출력하고 종료

환경 변수:
    RAYON_NUM_THREADS    --threads를 지정하지 않았을 때 스레드 수로 사용
";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// 스캔을 시작할 폴더. 없으면 현재 폴더.
    pub path: Option<PathBuf>,
    /// 사용자가 명시한 스레드 수 (없으면 기본값 사용)
    pub threads: Option<usize>,
    /// 시작할 때 켜 둘 감시 모드의 간격(초). 없으면 꺼 둔 채로 시작한다.
    pub watch: Option<u64>,
    /// 미리 전부 스캔해 둘 루트 폴더. 없으면 현재 폴더만 스캔한다.
    pub root: Option<PathBuf>,
    /// 크기를 1000 기준(Finder와 같음)으로 시작한다. 없으면 1024 기준.
    pub si: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Options),
    Help,
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut opts = Options::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--si" => opts.si = true,
            "-j" | "--threads" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} 옵션에는 값이 필요합니다"))?;
                opts.threads = Some(parse_threads(&value)?);
            }
            s if s.starts_with("--threads=") => {
                opts.threads = Some(parse_threads(&s["--threads=".len()..])?);
            }
            "-w" | "--watch" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} 옵션에는 값이 필요합니다"))?;
                opts.watch = Some(parse_watch(&value)?);
            }
            s if s.starts_with("--watch=") => {
                opts.watch = Some(parse_watch(&s["--watch=".len()..])?);
            }
            "-r" | "--root" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} 옵션에는 값이 필요합니다"))?;
                opts.root = Some(parse_root(&value)?);
            }
            s if s.starts_with("--root=") => {
                opts.root = Some(parse_root(&s["--root=".len()..])?);
            }
            // `--` 뒤는 전부 경로로 본다 (`-`로 시작하는 폴더 이름용)
            "--" => {
                for rest in args.by_ref() {
                    set_path(&mut opts, rest)?;
                }
            }
            s if !s.starts_with('-') => set_path(&mut opts, arg)?,
            _ => return Err(format!("알 수 없는 옵션: {arg}")),
        }
    }

    Ok(Parsed::Run(opts))
}

fn set_path(opts: &mut Options, value: String) -> Result<(), String> {
    if opts.path.is_some() {
        return Err("경로는 하나만 지정할 수 있습니다".into());
    }
    opts.path = Some(PathBuf::from(value));
    Ok(())
}

fn parse_threads(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("스레드 수는 1 이상의 정수여야 합니다: {value}")),
    }
}

fn parse_root(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("--root에는 폴더 경로가 필요합니다".into());
    }
    Ok(PathBuf::from(value))
}

fn parse_watch(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n >= MIN_WATCH_SECS => Ok(n),
        _ => Err(format!(
            "감시 간격은 {MIN_WATCH_SECS} 이상의 정수(초)여야 합니다: {value}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_runs_with_defaults() {
        assert_eq!(parse_strs(&[]), Ok(Parsed::Run(Options::default())));
    }

    #[test]
    fn watch_in_all_spellings_with_a_minimum_but_no_maximum() {
        let want = Ok(Parsed::Run(Options {
            watch: Some(10),
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["-w", "10"]), want);
        assert_eq!(parse_strs(&["--watch", "10"]), want);
        assert_eq!(parse_strs(&["--watch=10"]), want);
        // 최소값은 허용, 아주 큰 값도 허용
        assert!(parse_strs(&["-w", "2"]).is_ok());
        assert!(parse_strs(&["-w", "86400"]).is_ok());
        // 최소값 미만과 잘못된 값은 거부
        assert!(parse_strs(&["-w", "1"]).is_err());
        assert!(parse_strs(&["-w", "0"]).is_err());
        assert!(parse_strs(&["-w", "abc"]).is_err());
        assert!(parse_strs(&["-w"]).is_err());
    }

    #[test]
    fn threads_and_watch_combine() {
        let want = Ok(Parsed::Run(Options {
            threads: Some(4),
            watch: Some(5),
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["-j", "4", "-w", "5"]), want);
    }

    #[test]
    fn a_start_path_can_be_given_with_or_without_options() {
        let want = |path: &str| {
            Ok(Parsed::Run(Options {
                path: Some(PathBuf::from(path)),
                ..Options::default()
            }))
        };
        assert_eq!(parse_strs(&["/Users/me/Downloads"]), want("/Users/me/Downloads"));
        assert_eq!(parse_strs(&["."]), want("."));
        // 옵션과 섞어 써도, 순서가 달라도 된다
        let with_threads = Ok(Parsed::Run(Options {
            path: Some(PathBuf::from("dir")),
            threads: Some(4),
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["-j", "4", "dir"]), with_threads);
        assert_eq!(parse_strs(&["dir", "-j", "4"]), with_threads);
        // `--`로 `-`로 시작하는 폴더 이름도 쓸 수 있다
        assert_eq!(parse_strs(&["--", "-odd"]), want("-odd"));
    }

    #[test]
    fn only_one_start_path_is_allowed() {
        assert!(parse_strs(&["a", "b"]).is_err());
        assert!(parse_strs(&["a", "--", "b"]).is_err());
        assert!(parse_strs(&["--", "a", "b"]).is_err());
        // 값이 필요한 옵션 뒤의 값은 경로로 세지 않는다
        assert!(parse_strs(&["-r", "root", "dir"]).is_ok());
        assert!(parse_strs(&["-r", "root", "dir", "other"]).is_err());
        // 모르는 옵션은 여전히 오류
        assert!(parse_strs(&["-x"]).is_err());
        assert!(parse_strs(&["-"]).is_err());
    }

    #[test]
    fn si_switches_the_starting_base_and_mixes_with_other_arguments() {
        let want = Ok(Parsed::Run(Options {
            si: true,
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["--si"]), want);
        assert_eq!(
            parse_strs(&["--si", "dir"]),
            Ok(Parsed::Run(Options {
                si: true,
                path: Some(PathBuf::from("dir")),
                ..Options::default()
            }))
        );
        assert_eq!(parse_strs(&["dir", "--si"]), parse_strs(&["--si", "dir"]));
        // 값을 받는 옵션이 아니다
        assert!(parse_strs(&["--si=1"]).is_err());
        assert!(!matches!(parse_strs(&[]), Ok(Parsed::Run(Options { si: true, .. }))));
    }

    #[test]
    fn root_in_all_spellings() {
        let want = Ok(Parsed::Run(Options {
            root: Some(PathBuf::from("/Users/me")),
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["-r", "/Users/me"]), want);
        assert_eq!(parse_strs(&["--root", "/Users/me"]), want);
        assert_eq!(parse_strs(&["--root=/Users/me"]), want);
        assert!(parse_strs(&["-r"]).is_err());
        assert!(parse_strs(&["--root="]).is_err());
    }

    #[test]
    fn threads_in_all_spellings() {
        let want = Ok(Parsed::Run(Options {
            threads: Some(4),
            ..Options::default()
        }));
        assert_eq!(parse_strs(&["-j", "4"]), want);
        assert_eq!(parse_strs(&["--threads", "4"]), want);
        assert_eq!(parse_strs(&["--threads=4"]), want);
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse_strs(&["-h"]), Ok(Parsed::Help));
        assert_eq!(parse_strs(&["--threads", "4", "--help"]), Ok(Parsed::Help));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_strs(&["-j"]).is_err());
        assert!(parse_strs(&["-j", "0"]).is_err());
        assert!(parse_strs(&["-j", "abc"]).is_err());
        assert!(parse_strs(&["--threads=-3"]).is_err());
        assert!(parse_strs(&["--bogus"]).is_err());
    }
}

use crate::app::MIN_WATCH_SECS;

pub const HELP: &str = "\
mac-du - macOS 디스크 사용량 TUI

사용법:
    mac-du [옵션]

옵션:
    -j, --threads <N>    스캔에 쓸 스레드 수 (1 이상).
                         기본값은 min(CPU 코어 수, 16)입니다.
                         코어 수보다 크게 지정하면 스캔이 수 초씩 멈출 수 있습니다.
    -w, --watch <SEC>    시작하자마자 감시 모드를 켜고 SEC초마다 현재 폴더를 다시 스캔
                         (2 이상, 상한 없음). 큰 폴더라 스캔이 오래 걸리면 간격이 자동으로 늘어납니다.
                         앱 안에서는 w 키로 켜고 끄고, +/- 키로 간격을 바꿉니다.
    -h, --help           이 도움말을 출력하고 종료

환경 변수:
    RAYON_NUM_THREADS    --threads를 지정하지 않았을 때 스레드 수로 사용
";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// 사용자가 명시한 스레드 수 (없으면 기본값 사용)
    pub threads: Option<usize>,
    /// 시작할 때 켜 둘 감시 모드의 간격(초). 없으면 꺼 둔 채로 시작한다.
    pub watch: Option<u64>,
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
            _ => return Err(format!("알 수 없는 옵션: {arg}")),
        }
    }

    Ok(Parsed::Run(opts))
}

fn parse_threads(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("스레드 수는 1 이상의 정수여야 합니다: {value}")),
    }
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

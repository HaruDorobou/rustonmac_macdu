pub const HELP: &str = "\
mac-du - macOS 디스크 사용량 TUI

사용법:
    mac-du [옵션]

옵션:
    -j, --threads <N>    스캔에 쓸 스레드 수 (1 이상).
                         기본값은 min(CPU 코어 수, 16)입니다.
                         코어 수보다 크게 지정하면 스캔이 수 초씩 멈출 수 있습니다.
    -h, --help           이 도움말을 출력하고 종료

환경 변수:
    RAYON_NUM_THREADS    --threads를 지정하지 않았을 때 스레드 수로 사용
";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// 사용자가 명시한 스레드 수 (없으면 기본값 사용)
    pub threads: Option<usize>,
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
    fn threads_in_all_spellings() {
        let want = Ok(Parsed::Run(Options { threads: Some(4) }));
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

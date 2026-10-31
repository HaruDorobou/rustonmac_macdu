//! 바이트 수를 화면에 보여 줄 문자열로 바꾼다.
//!
//! 두 가지를 따로 고른다.
//! - 기준(`Base`): 1024 기준(`KiB`, `MiB`, `GiB`, `TiB`, `du -h`와 같음)과
//!   1000 기준(`KB`, `MB`, `GB`, `TB`, Finder와 같음)
//! - 단위(`Unit`): 값마다 알맞은 단위를 고르는 자동, 또는 K/M/G/T 중 하나로 고정
//!   (`top`에서 메모리 단위를 `e`로 돌리는 것과 같다)

/// 한 칸에 들어갈 수 있는 최대 글자 수. 고정 단위에서 이보다 길어지면 그 값만 자동 단위로 보여 준다.
const MAX_CELL_WIDTH: usize = 12;

const BINARY_LABELS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
const DECIMAL_LABELS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// 고정 단위일 때 소수점 아래 자릿수 (K, M, G, T 순). 어느 단위든 비슷한 정밀도가 되도록 맞췄다.
const FIXED_DECIMALS: [usize; 5] = [0, 1, 1, 2, 3];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Base {
    /// 1024 기준 (`KiB`, `MiB`, ...)
    #[default]
    Binary,
    /// 1000 기준 (`KB`, `MB`, ...)
    Decimal,
}

impl Base {
    fn factor(self) -> f64 {
        match self {
            Base::Binary => 1024.0,
            Base::Decimal => 1000.0,
        }
    }

    fn label(self, power: usize) -> &'static str {
        match self {
            Base::Binary => BINARY_LABELS[power],
            Base::Decimal => DECIMAL_LABELS[power],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unit {
    /// 값마다 알맞은 단위
    #[default]
    Auto,
    K,
    M,
    G,
    T,
}

impl Unit {
    /// 고정 단위의 거듭제곱 (K=1 ... T=4). 자동이면 `None`.
    fn power(self) -> Option<usize> {
        match self {
            Unit::Auto => None,
            Unit::K => Some(1),
            Unit::M => Some(2),
            Unit::G => Some(3),
            Unit::T => Some(4),
        }
    }

    fn next(self) -> Unit {
        match self {
            Unit::Auto => Unit::K,
            Unit::K => Unit::M,
            Unit::M => Unit::G,
            Unit::G => Unit::T,
            Unit::T => Unit::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Units {
    pub base: Base,
    pub unit: Unit,
}

impl Units {
    pub fn new(base: Base) -> Self {
        Units {
            base,
            unit: Unit::Auto,
        }
    }

    /// 자동 → K → M → G → T → 자동
    pub fn cycle_unit(&mut self) {
        self.unit = self.unit.next();
    }

    /// 1024 ↔ 1000
    pub fn toggle_base(&mut self) {
        self.base = match self.base {
            Base::Binary => Base::Decimal,
            Base::Decimal => Base::Binary,
        };
    }

    /// 목록의 한 칸에 넣을 크기 문자열.
    ///
    /// 고정 단위에서 값이 너무 커서 `MAX_CELL_WIDTH`를 넘으면, 그 값만 자동 단위로 보여 준다.
    /// 숫자마다 단위 라벨이 붙어 있어서 섞여도 헷갈리지 않는다.
    pub fn format(&self, bytes: u64) -> String {
        match self.unit.power() {
            None => auto(self.base, bytes, 1),
            Some(power) => {
                let s = fixed(self.base, bytes, power);
                if s.chars().count() > MAX_CELL_WIDTH {
                    auto(self.base, bytes, 1)
                } else {
                    s
                }
            }
        }
    }
}

/// 정확한 바이트 수와, 두 기준으로 읽은 값을 나란히. `12,158,771,200 B (11.32 GiB · 12.16 GB)`
pub fn exact(bytes: u64) -> String {
    let precise = format!("{} B", group(&bytes.to_string()));
    if bytes < 1000 {
        return precise;
    }
    format!(
        "{precise} ({} · {})",
        auto(Base::Binary, bytes, 2),
        auto(Base::Decimal, bytes, 2)
    )
}

/// 값에 맞는 단위를 골라 `decimals`자리까지 보여 준다.
fn auto(base: Base, bytes: u64, decimals: usize) -> String {
    let factor = base.factor();
    let mut value = bytes as f64;
    let mut power = 0;
    while value >= factor && power < 4 {
        value /= factor;
        power += 1;
    }
    if power == 0 {
        return format!("{} B", bytes);
    }
    // 반올림하면 한 단위 올라가는 값(1023.99 KiB → "1024.0 KiB")은 미리 올려서 보여 준다
    let scale = 10f64.powi(decimals as i32);
    if (value * scale).round() / scale >= factor && power < 4 {
        value /= factor;
        power += 1;
    }
    format!("{} {}", number(value, decimals), base.label(power))
}

/// `power`(K=1 ... T=4) 단위로 고정해서 보여 준다.
/// 0이 아닌데 표시 자릿수로는 0이 되는 값은 `<0.01 GiB`처럼 보여 줘서 비어 있는 것과 구분한다.
fn fixed(base: Base, bytes: u64, power: usize) -> String {
    let label = base.label(power);
    if bytes == 0 {
        return format!("0 {label}");
    }
    let decimals = FIXED_DECIMALS[power];
    let value = bytes as f64 / base.factor().powi(power as i32);
    let scale = 10f64.powi(decimals as i32);
    if (value * scale).round() == 0.0 {
        return format!("<{:.*} {label}", decimals, 1.0 / scale);
    }
    format!("{} {label}", number(value, decimals))
}

/// 소수점 `decimals`자리까지, 정수부는 천 단위로 끊어서
fn number(value: f64, decimals: usize) -> String {
    group(&format!("{:.*}", decimals, value))
}

/// `12345.6` → `12,345.6`
fn group(s: &str) -> String {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    match frac {
        Some(f) => format!("{out}.{f}"),
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: Units = Units {
        base: Base::Binary,
        unit: Unit::Auto,
    };
    const DEC: Units = Units {
        base: Base::Decimal,
        unit: Unit::Auto,
    };
    fn fixed_in(base: Base, unit: Unit) -> Units {
        Units { base, unit }
    }

    /// 앞서 `du -sk /Applications`와 한 바이트도 안 틀리게 일치했던 실제 값
    const APPLICATIONS: u64 = 12_158_771_200;

    #[test]
    fn auto_picks_the_unit_that_fits_the_value_in_both_bases() {
        assert_eq!(BIN.format(0), "0 B");
        assert_eq!(BIN.format(1023), "1023 B");
        assert_eq!(BIN.format(1024), "1.0 KiB");
        assert_eq!(BIN.format(140 * 1024), "140.0 KiB");
        assert_eq!(BIN.format(APPLICATIONS), "11.3 GiB");

        assert_eq!(DEC.format(999), "999 B");
        assert_eq!(DEC.format(1000), "1.0 KB");
        assert_eq!(DEC.format(APPLICATIONS), "12.2 GB");
    }

    #[test]
    fn the_same_bytes_read_differently_in_the_two_bases() {
        // Finder(1000 기준)와 du -h(1024 기준)의 숫자가 다른 이유
        assert_eq!(BIN.format(APPLICATIONS), "11.3 GiB");
        assert_eq!(DEC.format(APPLICATIONS), "12.2 GB");
    }

    #[test]
    fn a_value_that_rounds_up_to_the_next_unit_is_shown_in_that_unit() {
        // 1023.999 KiB를 그대로 반올림하면 "1024.0 KiB"가 된다
        assert_eq!(BIN.format(1_048_575), "1.0 MiB");
        assert_eq!(DEC.format(999_960), "1.0 MB"); // 999.96 KB → "1000.0 KB"가 되지 않게
        // 올림이 되지 않는 값은 그대로
        assert_eq!(BIN.format(1_048_000), "1,023.4 KiB");
    }

    #[test]
    fn a_fixed_unit_shows_every_row_in_the_same_unit() {
        let mib = fixed_in(Base::Binary, Unit::M);
        assert_eq!(mib.format(APPLICATIONS), "11,595.5 MiB");
        assert_eq!(mib.format(140 * 1024), "0.1 MiB");
        assert_eq!(mib.format(50 * 1024 * 1024), "50.0 MiB");

        assert_eq!(fixed_in(Base::Binary, Unit::G).format(APPLICATIONS), "11.32 GiB");
        assert_eq!(fixed_in(Base::Binary, Unit::T).format(APPLICATIONS), "0.011 TiB");
        assert_eq!(fixed_in(Base::Binary, Unit::K).format(140 * 1024), "140.0 KiB");

        assert_eq!(fixed_in(Base::Decimal, Unit::M).format(APPLICATIONS), "12,158.8 MB");
        assert_eq!(fixed_in(Base::Decimal, Unit::G).format(APPLICATIONS), "12.16 GB");
        assert_eq!(fixed_in(Base::Decimal, Unit::T).format(APPLICATIONS), "0.012 TB");
    }

    #[test]
    fn a_small_nonzero_value_is_not_shown_as_if_it_were_empty() {
        let gib = fixed_in(Base::Binary, Unit::G);
        assert_eq!(gib.format(0), "0 GiB");
        assert_eq!(gib.format(4096), "<0.01 GiB");
        assert_eq!(gib.format(5_000_000), "<0.01 GiB"); // 0.00466
        assert_eq!(gib.format(5_368_710), "0.01 GiB"); // 반올림하면 0.01이 되는 경계

        assert_eq!(fixed_in(Base::Binary, Unit::M).format(1), "<0.1 MiB");
        assert_eq!(fixed_in(Base::Binary, Unit::T).format(1_000_000), "<0.001 TiB");
        assert_eq!(fixed_in(Base::Decimal, Unit::K).format(1), "<0.1 KB");
    }

    #[test]
    fn a_value_too_wide_for_a_fixed_unit_falls_back_to_auto_for_that_row_only() {
        let mib = fixed_in(Base::Binary, Unit::M);
        // 3 TiB를 MiB로 쓰면 "3,145,728.0 MiB"(15자)라서 그 값만 자동 단위로
        assert_eq!(mib.format(3 * 1024u64.pow(4)), "3.0 TiB");
        // 같은 모드의 작은 값은 그대로 MiB
        assert_eq!(mib.format(5 * 1024 * 1024), "5.0 MiB");

        let kib = fixed_in(Base::Binary, Unit::K);
        assert_eq!(kib.format(1024u64.pow(3)), "1.0 GiB");
        assert_eq!(kib.format(APPLICATIONS), "11.3 GiB");

        // 12자까지는 그대로, 13자부터 대체한다
        assert_eq!(kib.format(102_399_898), "99,999.9 KiB"); // 12자
        assert_eq!(kib.format(102_400_000), "97.7 MiB"); // "100,000.0 KiB"는 13자
    }

    #[test]
    fn the_unit_cycles_through_auto_k_m_g_t_and_the_base_toggles_independently() {
        let mut u = Units::default();
        let seen: Vec<Unit> = (0..6)
            .map(|_| {
                let now = u.unit;
                u.cycle_unit();
                now
            })
            .collect();
        assert_eq!(seen, [Unit::Auto, Unit::K, Unit::M, Unit::G, Unit::T, Unit::Auto]);

        assert_eq!(u.unit, Unit::K, "a sixth press lands on K again");
        u.toggle_base();
        assert_eq!((u.base, u.unit), (Base::Decimal, Unit::K), "toggling the base keeps the unit");
        u.cycle_unit(); // M
        assert_eq!(u.format(5 * 1_000_000), "5.0 MB");
        u.toggle_base();
        assert_eq!((u.base, u.unit), (Base::Binary, Unit::M));
        assert_eq!(Units::new(Base::Decimal).base, Base::Decimal);
    }

    #[test]
    fn exact_shows_every_byte_and_both_readings() {
        assert_eq!(exact(APPLICATIONS), "12,158,771,200 B (11.32 GiB · 12.16 GB)");
        assert_eq!(exact(1500), "1,500 B (1.46 KiB · 1.50 KB)");
        assert_eq!(exact(512), "512 B");
        assert_eq!(exact(0), "0 B");
        assert_eq!(exact(1_048_575), "1,048,575 B (1.00 MiB · 1.05 MB)");
    }

    #[test]
    fn grouping_handles_short_long_and_fractional_numbers() {
        assert_eq!(group("0"), "0");
        assert_eq!(group("999"), "999");
        assert_eq!(group("1000"), "1,000");
        assert_eq!(group("1234567.89"), "1,234,567.89");
        assert_eq!(group("12345"), "12,345");
    }
}

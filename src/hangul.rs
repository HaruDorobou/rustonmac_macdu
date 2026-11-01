//! 한글 자판(두벌식) 상태에서 눌린 키를 같은 위치의 영문 키로 되돌린다.
//!
//! 한글 입력 상태에서는 `j`가 `ㅓ`로, `hl`이 `ㅚ`로 들어와서 단축키가 먹지 않는다.
//! 글자 입력 칸이 아닐 때만 이 변환을 쓰므로, 검색어에는 한글이 그대로 들어간다.
//! 터미널은 Shift 조합 중 모양이 달라지는 것(`ㅃㅉㄸㄲㅆㅒㅖ`)만 구분할 수 있다.
//! 예를 들어 Shift+`ㅎ`(G)도 `ㅎ`으로 오므로 소문자 `g`로 처리된다.

/// 호환 자모 하나가 눌린 키. 대문자는 Shift를 누른 것이다.
fn jamo_keys(c: char) -> Option<&'static str> {
    Some(match c {
        'ㄱ' => "r", 'ㄲ' => "R", 'ㄴ' => "s", 'ㄷ' => "e", 'ㄸ' => "E", 'ㄹ' => "f",
        'ㅁ' => "a", 'ㅂ' => "q", 'ㅃ' => "Q", 'ㅅ' => "t", 'ㅆ' => "T", 'ㅇ' => "d",
        'ㅈ' => "w", 'ㅉ' => "W", 'ㅊ' => "c", 'ㅋ' => "z", 'ㅌ' => "x", 'ㅍ' => "v",
        'ㅎ' => "g",
        'ㅏ' => "k", 'ㅐ' => "o", 'ㅑ' => "i", 'ㅒ' => "O", 'ㅓ' => "j", 'ㅔ' => "p",
        'ㅕ' => "u", 'ㅖ' => "P", 'ㅗ' => "h", 'ㅛ' => "y", 'ㅜ' => "n", 'ㅠ' => "b",
        'ㅡ' => "m", 'ㅣ' => "l",
        // 겹모음과 겹받침은 키 두 개를 연달아 누른 것이다
        'ㅘ' => "hk", 'ㅙ' => "ho", 'ㅚ' => "hl", 'ㅝ' => "nj", 'ㅞ' => "np", 'ㅟ' => "nl",
        'ㅢ' => "ml",
        'ㄳ' => "rt", 'ㄵ' => "sw", 'ㄶ' => "sg", 'ㄺ' => "fr", 'ㄻ' => "fa", 'ㄼ' => "fq",
        'ㄽ' => "ft", 'ㄾ' => "fx", 'ㄿ' => "fv", 'ㅀ' => "fg", 'ㅄ' => "qt",
        _ => return None,
    })
}

const INITIALS: [char; 19] = [
    'ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ', 'ㅌ',
    'ㅍ', 'ㅎ',
];
const MEDIALS: [char; 21] = [
    'ㅏ', 'ㅐ', 'ㅑ', 'ㅒ', 'ㅓ', 'ㅔ', 'ㅕ', 'ㅖ', 'ㅗ', 'ㅘ', 'ㅙ', 'ㅚ', 'ㅛ', 'ㅜ', 'ㅝ', 'ㅞ', 'ㅟ',
    'ㅠ', 'ㅡ', 'ㅢ', 'ㅣ',
];
/// 받침 없음은 `None`
const FINALS: [Option<char>; 28] = [
    None, Some('ㄱ'), Some('ㄲ'), Some('ㄳ'), Some('ㄴ'), Some('ㄵ'), Some('ㄶ'), Some('ㄷ'),
    Some('ㄹ'), Some('ㄺ'), Some('ㄻ'), Some('ㄼ'), Some('ㄽ'), Some('ㄾ'), Some('ㄿ'), Some('ㅀ'),
    Some('ㅁ'), Some('ㅂ'), Some('ㅄ'), Some('ㅅ'), Some('ㅆ'), Some('ㅇ'), Some('ㅈ'), Some('ㅊ'),
    Some('ㅋ'), Some('ㅌ'), Some('ㅍ'), Some('ㅎ'),
];

/// 글자 하나를 같은 키 위치의 영문 키들로 바꾼다. 한글이 아니면 `None`.
/// 자모는 키 하나(겹모음·겹받침은 둘), 완성된 글자(`하`, `걱`)는 초성·중성·종성 순서로 푼다.
/// 한글 입력기는 `gk`를 `하`로, `hl`을 `ㅚ`로 합쳐서 보내기 때문이다.
pub fn to_keys(c: char) -> Option<Vec<char>> {
    if let Some(keys) = jamo_keys(c) {
        return Some(keys.chars().collect());
    }
    let code = (c as u32).checked_sub(0xAC00).filter(|&n| n < 11172)? as usize;
    let (initial, medial, fin) = (code / 588, (code % 588) / 28, code % 28);
    let mut out: Vec<char> = Vec::new();
    for jamo in [Some(INITIALS[initial]), Some(MEDIALS[medial]), FINALS[fin]].into_iter().flatten() {
        out.extend(jamo_keys(jamo)?.chars());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(c: char) -> String {
        to_keys(c).map(|v| v.into_iter().collect()).unwrap_or_default()
    }

    #[test]
    fn single_jamo_map_to_the_same_physical_key() {
        // vim 이동 키 hjkl, 그리고 자주 쓰는 q r w e
        assert_eq!(
            [keys('ㅗ'), keys('ㅓ'), keys('ㅏ'), keys('ㅣ')],
            ["h", "j", "k", "l"]
        );
        assert_eq!([keys('ㅂ'), keys('ㄱ'), keys('ㅈ'), keys('ㄷ')], ["q", "r", "w", "e"]);
        assert_eq!([keys('ㅁ'), keys('ㄴ'), keys('ㅇ'), keys('ㄹ'), keys('ㅎ')], ["a", "s", "d", "f", "g"]);
        // Shift로 모양이 달라지는 키는 대문자가 된다 (O는 Finder에서 현재 폴더 열기)
        assert_eq!([keys('ㅒ'), keys('ㅃ')], ["O", "Q"]);
    }

    #[test]
    fn syllables_the_input_method_merged_are_split_back_into_keys() {
        // 자음 없이 hl 을 치면 ㅚ, hk 는 ㅘ 로 합쳐진다
        assert_eq!(keys('ㅚ'), "hl");
        assert_eq!(keys('ㅘ'), "hk");
        // 자음이 있으면 완성된 글자: gk 는 하, dhl 은 외 (ㅇ도 d 키다)
        assert_eq!(keys('하'), "gk");
        assert_eq!(keys('외'), "dhl");
        // 받침도 키 하나: 걱 = r j r
        assert_eq!(keys('걱'), "rjr");
        // 겹받침과 겹모음
        assert_eq!(keys('닭'), "ekfr");
        assert_eq!(keys('의'), "dml");
        assert_eq!(keys('뷁'), "qnpfr");
        // 된소리는 Shift
        assert_eq!(keys('까'), "Rk");
    }

    #[test]
    fn anything_else_is_left_alone() {
        for c in ['a', 'Z', '/', '1', ' ', '日', 'é'] {
            assert_eq!(to_keys(c), None, "{c:?}");
        }
        // 한글 음절 영역의 처음과 끝
        assert_eq!(keys('가'), "rk");
        assert_eq!(keys('힣'), "glg");
    }
}

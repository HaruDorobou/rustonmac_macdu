//! 지금 선택된 키보드 입력 소스(영문, 한글 등)를 읽는다.
//!
//! macOS의 Text Input Sources API를 그대로 부른다. 터미널 안의 일반 프로그램에서도 별도 권한 없이
//! 동작하고, 두 번째 호출부터는 마이크로초 단위라 매 프레임 물어봐도 된다.
//! 화면 입력은 건드리지 않고 읽기만 한다. 화면 그리기와 같은 메인 스레드에서 불러야 한다.

/// 입력 소스를 이 프로그램이 구분하는 만큼만 나눈 것. 다국어를 지원하려는 것이 아니므로
/// 영문과 한글 말고는 모두 `Other`다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    English,
    Korean,
    Other,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::English => "영문",
            Source::Korean => "한글",
            Source::Other => "기타",
        }
    }
}

/// 영문 자판으로 치는 키보드 배열
const ENGLISH_LAYOUTS: [&str; 12] = [
    "ABC", "US", "USExtended", "British", "Australian", "Canadian", "Irish", "Dvorak", "Dvorak-Left",
    "Dvorak-Right", "Colemak", "ABC-India",
];

/// 입력 소스 ID(예: `com.apple.inputmethod.Korean.2SetKorean`)를 분류한다.
pub fn classify(id: &str) -> Source {
    let lower = id.to_lowercase();
    // 애플 한글 입력기와, 구름 같은 다른 한글 입력기
    if lower.contains("korean") || lower.contains("hangul") || lower.contains(".han2") || lower.contains(".han3") {
        return Source::Korean;
    }
    match id.strip_prefix("com.apple.keylayout.") {
        Some(layout) if ENGLISH_LAYOUTS.contains(&layout) => Source::English,
        _ => Source::Other,
    }
}

/// 지금 선택된 입력 소스. 읽을 수 없으면 `None`.
#[cfg(target_os = "macos")]
pub fn current() -> Option<Source> {
    use std::ffi::{c_char, c_void, CStr};

    type CFTypeRef = *const c_void;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISPropertyInputSourceID: CFTypeRef;
        fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
        fn TISGetInputSourceProperty(src: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFRelease(r: CFTypeRef);
        fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source: bool) -> i32;
    }
    const UTF8: u32 = 0x0800_0100;

    // SAFETY: 반환된 소스는 복사본이라 직접 해제해야 하고, ID 문자열은 소스가 살아 있는 동안만
    // 유효하므로 해제 전에 버퍼로 복사한다. 버퍼 크기를 넘기므로 넘쳐 쓰지 않는다.
    unsafe {
        // 입력 소스가 바뀌었다는 알림은 런루프로 들어온다. 터미널 프로그램은 런루프를 돌리지 않아
        // 그대로 읽으면 처음 값이 계속 나오므로, 읽기 전에 쌓인 알림을 한 번 처리한다.
        // 기다리지 않고(0초) 바로 돌아온다.
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, true);
        let src = TISCopyCurrentKeyboardInputSource();
        if src.is_null() {
            return None;
        }
        let id = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
        let mut buf = [0 as c_char; 256];
        let copied = !id.is_null() && CFStringGetCString(id, buf.as_mut_ptr(), buf.len() as isize, UTF8);
        let text = copied.then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned());
        CFRelease(src);
        text.map(|t| classify(&t))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn current() -> Option<Source> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_source_ids_are_sorted_into_english_korean_and_other() {
        for id in ["com.apple.keylayout.ABC", "com.apple.keylayout.US", "com.apple.keylayout.Dvorak"] {
            assert_eq!(classify(id), Source::English, "{id}");
        }
        for id in [
            "com.apple.inputmethod.Korean.2SetKorean",
            "com.apple.inputmethod.Korean.390Sebulshik",
            "org.youknowone.inputmethod.Gureum.han2",
        ] {
            assert_eq!(classify(id), Source::Korean, "{id}");
        }
        for id in [
            "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese",
            "com.apple.inputmethod.Japanese",
            "com.apple.keylayout.Russian",
            "com.apple.inputmethod.SCIM.ITABC",
            "",
        ] {
            assert_eq!(classify(id), Source::Other, "{id:?}");
        }
    }

    #[test]
    fn reading_the_current_source_does_not_crash() {
        // 값은 실행하는 사람의 설정에 달려 있어서 호출이 끝나는 것만 확인한다
        let _ = current();
        let _ = current();
    }
}

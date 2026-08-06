//! nvim の `+` / `*` レジスタを OS クリップボードへ繋ぐ口（DESIGN §5.4）。
//!
//! 同梱 nvim は headless で、win32yank のような外部プロバイダを持たない。代わりに
//! nvim 側の `g:clipboard` を host への `rpcrequest` として実装し、その受け口を
//! host が持つ。RPC の受け口（[`crate::server`]）が core にある以上、口の定義も
//! core に置くしかない。**実体は OS 側**（Win32 クリップボードを直に叩く
//! `anvi` crate の実装）で、core はトレイトと、テキストとレジスタの相互変換だけを持つ。
//!
//! 変換の要は「末尾改行 = 行指向」という 1 点だが、それが要るのは**取得方向だけ**
//! である。Windows 側でコピーしたテキストが `\r\n` で終わっていれば行単位のコピーと
//! みなして nvim では `V` レジスタにする。
//!
//! 逆のコピー方向では regtype を見る必要が一切ない。nvim は行指向・矩形指向のとき
//! `lines` の末尾に空文字列を入れて渡してくるので、host は CRLF で結合するだけで
//! 末尾改行が自然に付く。実測（同梱 nvim が `g:clipboard` の `copy` に渡す値）:
//!
//! - `"+yy`（1 行・行指向）  → `lines = {"abc", ""}`, `regtype = "V"`
//! - `"+yj`（2 行・行指向）  → `lines = {"abc", "def", ""}`, `regtype = "V"`
//! - `"+yw`（文字指向）      → `lines = {"abc"}`, `regtype = "v"`
//! - 矩形（`<C-v>` で 2 行） → `lines = {"ab", "ef", ""}`, `regtype = "b"`
//!
//! 矩形の regtype は `setreg()` の綴り（`"\x16{幅}"`）ではなく `"b"` で来る、という
//! のもこの実測で分かった点である。regtype を解釈して CRLF を「足す」実装にすると
//! 行指向のヤンクで改行が二重になるので、コピー方向は regtype を受け取らない。

use std::sync::Mutex;

/// nvim の `+` / `*` レジスタが載る OS クリップボードへの口。実体は OS 側（`anvi` crate）。
pub trait Clipboard: std::fmt::Debug + Send + Sync + 'static {
    /// クリップボードのテキストを読む。テキストが載っていなければ空文字列。
    fn get(&self) -> anyhow::Result<String>;
    /// クリップボードのテキストを置き換える。
    fn set(&self, text: &str) -> anyhow::Result<()>;
}

/// nvim のレジスタ種別。取得方向で nvim へ返すためだけに持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegType {
    /// 文字指向（`v`）。
    Char,
    /// 行指向（`V`）。
    Line,
}

impl RegType {
    /// nvim へ返す regtype（`setreg()` の綴り）。
    #[must_use]
    pub fn as_nvim(self) -> &'static str {
        match self {
            Self::Char => "v",
            Self::Line => "V",
        }
    }
}

/// クリップボードの生テキストを nvim のレジスタへ。末尾改行があれば行指向。
///
/// 行分割は [`crate::text::to_lines`] に委ねる（末尾改行は終端子として落ちる）。
/// 落とした末尾改行の情報はここで regtype に移し替える。
#[must_use]
pub fn to_register(raw: &str) -> (Vec<String>, RegType) {
    let regtype = if raw.ends_with('\n') || raw.ends_with('\r') {
        RegType::Line
    } else {
        RegType::Char
    };
    (crate::text::to_lines(raw), regtype)
}

/// プロセス内クリップボード。Win32 を持たない環境（core のテストと `examples/step1`）用。
#[derive(Debug, Default)]
pub struct Memory(Mutex<String>);

impl Memory {
    /// 今載っているテキスト。
    #[must_use]
    pub fn contents(&self) -> String {
        self.0
            .lock()
            .expect("memory clipboard mutex poisoned")
            .clone()
    }

    /// テキストを載せる。
    pub fn put(&self, text: &str) {
        let mut slot = self.0.lock().expect("memory clipboard mutex poisoned");
        slot.clear();
        slot.push_str(text);
    }
}

impl Clipboard for Memory {
    fn get(&self) -> anyhow::Result<String> {
        Ok(self.contents())
    }

    fn set(&self, text: &str) -> anyhow::Result<()> {
        self.put(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Clipboard as _, Memory, RegType, to_register};
    use crate::text::to_crlf;

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_string()).collect()
    }

    /// 末尾改行の無いテキストは文字指向。
    #[test]
    fn no_trailing_break_is_charwise() {
        assert_eq!(to_register("hello"), (v(&["hello"]), RegType::Char));
        assert_eq!(to_register("a\r\nb"), (v(&["a", "b"]), RegType::Char));
    }

    /// 末尾改行があれば行指向。CRLF / LF / CR のどれでも同じ。
    #[test]
    fn a_trailing_break_is_linewise() {
        assert_eq!(to_register("a\r\n"), (v(&["a"]), RegType::Line));
        assert_eq!(to_register("a\n"), (v(&["a"]), RegType::Line));
        assert_eq!(to_register("a\r"), (v(&["a"]), RegType::Line));
        assert_eq!(to_register("a\r\nb\r\n"), (v(&["a", "b"]), RegType::Line));
    }

    /// 空のクリップボードは「空行 1 本の文字指向」。nvim のレジスタは必ず 1 行以上持つ。
    #[test]
    fn an_empty_clipboard_is_a_single_empty_charwise_line() {
        assert_eq!(to_register(""), (v(&[""]), RegType::Char));
    }

    /// UTF-8（日本語）はそのまま通る。
    #[test]
    fn multibyte_text_survives_both_directions() {
        assert_eq!(
            to_register("日本語\r\n改行"),
            (v(&["日本語", "改行"]), RegType::Char)
        );
        assert_eq!(to_crlf(&v(&["日本語", "改行"])), "日本語\r\n改行");
    }

    /// nvim が `copy` に渡してくる形をそのまま [`crate::text::to_crlf`] で結合する
    /// だけで、指向ごと往復する。行指向・矩形指向では nvim が末尾に空行を入れて
    /// くるので、regtype を見て CRLF を足す必要は無い——という規約をここで固定する
    /// （足すと改行が二重になる）。
    #[test]
    fn the_nvim_yank_shape_round_trips_without_a_regtype() {
        for (yanked, raw, expected) in [
            // `"+yy`（1 行・行指向）
            (v(&["abc", ""]), "abc\r\n", (v(&["abc"]), RegType::Line)),
            // `"+yw`（文字指向）
            (v(&["abc"]), "abc", (v(&["abc"]), RegType::Char)),
            // `"+yj`（2 行・行指向）
            (
                v(&["a", "b", ""]),
                "a\r\nb\r\n",
                (v(&["a", "b"]), RegType::Line),
            ),
            // 矩形（regtype は `"b"` で来るが、末尾の空行があるので行指向に落ちる）
            (
                v(&["ab", "ef", ""]),
                "ab\r\nef\r\n",
                (v(&["ab", "ef"]), RegType::Line),
            ),
        ] {
            let out = to_crlf(&yanked);
            assert_eq!(out, raw, "yanked = {yanked:?}");
            assert_eq!(to_register(&out), expected, "raw = {out:?}");
        }
    }

    /// 取得方向の regtype は nvim の綴りで返る。
    #[test]
    fn regtype_spells_itself_the_way_nvim_does() {
        assert_eq!(RegType::Char.as_nvim(), "v");
        assert_eq!(RegType::Line.as_nvim(), "V");
    }

    #[test]
    fn the_memory_clipboard_stores_what_it_is_given() {
        let clip = Memory::default();
        assert_eq!(clip.get().unwrap(), "");
        clip.set("日本語\r\n").unwrap();
        assert_eq!(clip.get().unwrap(), "日本語\r\n");
        assert_eq!(clip.contents(), "日本語\r\n");
        clip.put("overwritten");
        assert_eq!(clip.get().unwrap(), "overwritten");
    }
}

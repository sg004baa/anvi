//! 改行コードの正規化（DESIGN §8.4）。
//!
//! 取得時は `\r\n` / `\n` / `\r` のいずれも行区切りとして扱い、内部表現は常に
//! 行配列（nvim バッファの行）にする。書き戻し時は `\r\n` で結合する。

/// 生テキストを行配列へ分割する。
///
/// `\r\n` / `\n` / `\r` のいずれも行区切りとして扱う。**末尾の改行は「終端子」として
/// 扱い、空行を生まない**（`"a\r\n"` → `["a"]`）。入力欄から取得したテキストは末尾に
/// 改行を含むことがあり（`TextPattern` の `DocumentRange`、ブラウザの
/// contenteditable を `Ctrl+C` した場合など）、それをそのまま行配列にすると
/// 「1 行しか入っていない欄なのに nvim では 2 行」になり、書き戻しで存在しなかった
/// 改行が生える。Slack / Discord ではそれが連投事故になる（DESIGN §10.2）。
/// 落とすのは末尾の 1 つだけなので、意図的な末尾の空行（`"a\n\n"` → `["a", ""]`）は残る。
/// 空文字列は `[""]` になる（nvim バッファは必ず 1 行以上持つ）。
#[must_use]
pub fn to_lines(raw: &str) -> Vec<String> {
    // `\r` / `\n` は ASCII なので UTF-8 の多バイト列の内部には現れない。
    // よってバイト走査でスライスしても文字境界を割らない。
    let bytes = raw.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(raw[start..i].to_owned());
                i += 1;
                start = i;
            }
            b'\r' => {
                lines.push(raw[start..i].to_owned());
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    lines.push(raw[start..].to_owned());
    // 末尾の改行 1 つ分が生んだ空行を落とす（上記の終端子扱い）。
    if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// 行配列を `\r\n` 区切りの 1 本のテキストへ結合する。
///
/// `CF_UNICODETEXT` の慣習および旧来の EDIT コントロールの都合により `\r\n` を
/// 使う。**末尾には改行を付けない。** 取得時に末尾改行を終端子として落としている
/// ため（[`to_lines`]）、書き戻しでも復活させない。
#[must_use]
pub fn to_crlf(lines: &[String]) -> String {
    let capacity = lines
        .iter()
        .map(|line| line.len() + 2)
        .sum::<usize>()
        .saturating_sub(2);
    let mut out = String::with_capacity(capacity);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{to_crlf, to_lines};

    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_string_is_a_single_empty_line() {
        assert_eq!(to_lines(""), v(&[""]));
    }

    #[test]
    fn no_line_break_is_a_single_line() {
        assert_eq!(to_lines("hello"), v(&["hello"]));
    }

    #[test]
    fn splits_lf() {
        assert_eq!(to_lines("a\nb\nc"), v(&["a", "b", "c"]));
    }

    #[test]
    fn splits_crlf() {
        assert_eq!(to_lines("a\r\nb\r\nc"), v(&["a", "b", "c"]));
    }

    #[test]
    fn splits_bare_cr() {
        assert_eq!(to_lines("a\rb\rc"), v(&["a", "b", "c"]));
    }

    #[test]
    fn splits_mixed_breaks() {
        assert_eq!(to_lines("a\r\nb\nc\rd"), v(&["a", "b", "c", "d"]));
    }

    #[test]
    fn one_trailing_break_is_a_terminator() {
        assert_eq!(to_lines("a\r\n"), v(&["a"]));
        assert_eq!(to_lines("a\n"), v(&["a"]));
        assert_eq!(to_lines("a\r"), v(&["a"]));
        assert_eq!(to_lines("\n"), v(&[""]));
    }

    #[test]
    fn an_intentional_trailing_blank_line_survives() {
        assert_eq!(to_lines("a\n\n"), v(&["a", ""]));
        assert_eq!(to_lines("a\r\n\r\n"), v(&["a", ""]));
    }

    #[test]
    fn leading_and_consecutive_breaks_yield_empty_lines() {
        assert_eq!(to_lines("\na\n\nb"), v(&["", "a", "", "b"]));
        assert_eq!(to_lines("\r\n\r\n"), v(&["", ""]));
    }

    #[test]
    fn keeps_multibyte_text_intact() {
        assert_eq!(
            to_lines("日本語\r\n改行\nテスト"),
            v(&["日本語", "改行", "テスト"])
        );
    }

    #[test]
    fn crlf_joins_without_trailing_break() {
        assert_eq!(to_crlf(&v(&["a", "b"])), "a\r\nb");
        assert_eq!(to_crlf(&v(&["only"])), "only");
        assert_eq!(to_crlf(&v(&[])), "");
        assert_eq!(to_crlf(&v(&["", ""])), "\r\n");
    }

    /// 末尾改行を含まないテキストは完全に往復する。
    #[test]
    fn round_trip_is_exact_without_a_trailing_break() {
        for raw in ["", "hello", "a\r\nb\r\nc", "日本語\r\nの\r\n行"] {
            let lines = to_lines(raw);
            assert_eq!(to_crlf(&lines), raw, "round trip failed for {raw:?}");
            assert_eq!(to_lines(&to_crlf(&lines)), lines);
        }
    }

    /// 末尾改行は終端子なので往復で消える。これは意図した非対称性である
    /// （入力欄に無かった改行を書き戻しで生やさないため）。
    #[test]
    fn a_trailing_break_is_dropped_by_the_round_trip() {
        assert_eq!(to_crlf(&to_lines("a\r\n")), "a");
        assert_eq!(to_crlf(&to_lines("a\n\n")), "a\r\n");
        assert_eq!(to_crlf(&to_lines("\r\n\r\n")), "\r\n");
    }

    #[test]
    fn round_trip_normalizes_lf_and_cr_to_crlf() {
        assert_eq!(to_crlf(&to_lines("a\nb\rc")), "a\r\nb\r\nc");
    }
}

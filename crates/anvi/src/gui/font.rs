//! `guifont` の解釈（DESIGN v2 §4.4）。
//!
//! ここは Win32 に一切触らない純粋な文字列処理なので、開発機（Linux）でもテストが回る。
//!
//! 掟はひとつだけ: **解けない指定を既定へ黙って落とさない。** 黙って別のフォントで
//! 描いてしまうと、利用者は `guifont` が効いていないことに気づけない。解けなければ
//! [`GuiFont::Invalid`] を返し、呼び出し側が警告を出して現状維持する。

/// 同梱フォントのファミリ名。フォールバック鎖の最後尾であり、既定でもある。
///
/// exe に埋め込んで [`crate::gui::render`] が独自のフォントコレクションとして
/// DirectWrite へ渡すので、**利用者の環境に何が入っていようと必ず存在する**。
/// 等幅で日本語まで持っているため、CJK の最終手段もこれで足りる。
/// 名前は ttf の name テーブル（ID 1）と一致させること。
pub const BUNDLED_FAMILY: &str = "Moralerspace Argon HW";

/// フォールバック鎖の最後尾。同梱フォントなので「入っていない」ことがない。
const LAST_RESORT_FAMILY: &str = BUNDLED_FAMILY;

/// 既定のフォントサイズ（pt）。
const DEFAULT_SIZE_PT: f32 = 12.0;

/// 受け付けるサイズの下限・上限（pt）。
///
/// 桁を打ち間違えた `guifont`（`:h1200` など）でセル寸法が跳ね上がると、
/// レンダーターゲットの再確保だけで固まったように見える。範囲外は素直に弾く。
const MIN_SIZE_PT: f32 = 1.0;
const MAX_SIZE_PT: f32 = 400.0;

/// `option_set guifont` の解釈結果。
#[derive(Clone, Debug, PartialEq)]
pub enum GuiFont {
    /// 「GUI に任せる」。空文字列、またはサイズを 1 つも含まない候補列。
    ///
    /// 後者は **nvim 0.12 の組み込み既定値**
    /// （`"Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace"`）がこれで、
    /// 利用者が何も選んでいないことを意味する。警告を出す筋合いはないし、
    /// 勝手にサイズを当てはめて別のフォントで描くのも違う。
    Unspecified,
    /// 解けた指定。
    Spec(FontSpec),
    /// 解けない指定。呼び出し側が警告を出して現状維持する。
    Invalid,
}

/// 描画に使うフォントの指定。
///
/// `families` は `guifont` のカンマ区切り候補列を順に並べたもので、`_` は空白へ
/// 読み替え済み。**先頭から順に「実在する最初のもの」がプライマリ**
/// （→ [`FontSpec::primary`]）。描画側は鎖（候補列 + 最終手段）を
/// [`FontSpec::fallback_chain`] で受け取る。
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub families: Vec<String>,
    pub size_pt: f32,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self {
            families: vec![LAST_RESORT_FAMILY.to_owned()],
            size_pt: DEFAULT_SIZE_PT,
        }
    }
}

impl FontSpec {
    /// nvim の `guifont`（例 `"UDEV_Gothic:h12"`, `"A:h12,B:h12"`）を解く。
    ///
    /// 受け付ける形は `ファミリ名[:h<サイズ>]` のカンマ区切り。
    ///
    /// - `_` は空白として読む（vim 由来の記法）。空白をそのまま含む指定も通る
    ///   （`:set guifont=MS\ Gothic:h11` のバックスラッシュは ex コマンド側で
    ///   消費されるので、オプション値には生の空白が入っている）。
    /// - サイズは **候補列全体で最初に現れたものを採る**。nvim は実際に使われた
    ///   フォントのサイズを採るが、どれが使われるかは DirectWrite に訊くまで
    ///   分からない。食い違うサイズを並べたときの結果を決め打ちで固定しておく。
    /// - サイズがどこにも無ければ [`GuiFont::Unspecified`]（= GUI に任せる）。
    /// - `:b` / `:i` のような他のオプションが付いていたら [`GuiFont::Invalid`]。
    ///   無視して描くと利用者の指定と画面が食い違う。
    #[must_use]
    pub fn parse(guifont: &str) -> GuiFont {
        let spec = guifont.trim();
        if spec.is_empty() {
            return GuiFont::Unspecified;
        }

        let mut families = Vec::new();
        let mut size_pt = None;
        for entry in spec.split(',') {
            let mut parts = entry.split(':');
            let Some(family) = parts.next() else {
                return GuiFont::Invalid;
            };
            let family = family.replace('_', " ");
            let family = family.trim();
            if family.is_empty() {
                return GuiFont::Invalid;
            }

            let mut entry_size = None;
            for opt in parts {
                let Some(digits) = opt.strip_prefix('h') else {
                    return GuiFont::Invalid;
                };
                if entry_size.is_some() {
                    // `:h12:h14` のような矛盾した指定。どちらを採っても嘘になる。
                    return GuiFont::Invalid;
                }
                let Ok(value) = digits.parse::<f32>() else {
                    return GuiFont::Invalid;
                };
                if !(MIN_SIZE_PT..=MAX_SIZE_PT).contains(&value) {
                    return GuiFont::Invalid;
                }
                entry_size = Some(value);
            }

            families.push(family.to_owned());
            size_pt = size_pt.or(entry_size);
        }

        match size_pt {
            Some(size_pt) => GuiFont::Spec(Self { families, size_pt }),
            // サイズを 1 つも持たない候補列は「利用者は何も選んでいない」。
            None => GuiFont::Unspecified,
        }
    }

    /// 候補列のうち **実在する最初のファミリ**。
    ///
    /// `exists` はファミリ名を DirectWrite のコレクションで引く述語。どれも無ければ
    /// `None` を返し、呼び出し側が落とす（黙って別のフォントで描かない）。
    #[must_use]
    pub fn primary(&self, exists: impl Fn(&str) -> bool) -> Option<&str> {
        self.families
            .iter()
            .map(String::as_str)
            .find(|family| exists(family))
    }

    /// DirectWrite に渡すファミリ鎖。前から順に「使えるものを使う」。
    ///
    /// 利用者指定 → 同梱フォントの順に並べ、CJK が確実に出る等幅を最後に置く。
    /// 指定フォントが英数字しか持っていなくても、日本語は最後尾が拾う。
    ///
    /// DirectWrite の `CreateTextFormat` はカンマ区切りのファミリ列を解釈しない
    /// （Silverlight や CSS とは違う）。この文字列はレンダラ側が自分で分解し、
    /// 実在する先頭要素をプライマリとして解決し、残りを `IDWriteFontFallbackBuilder`
    /// のマッピングに積む。区切りの取り決めをここに閉じ込めるための形である。
    #[must_use]
    pub fn fallback_chain(&self) -> String {
        let mut chain: Vec<&str> = self.families.iter().map(String::as_str).collect();
        if !chain
            .iter()
            .any(|family| family.eq_ignore_ascii_case(LAST_RESORT_FAMILY))
        {
            chain.push(LAST_RESORT_FAMILY);
        }
        chain.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(guifont: &str) -> FontSpec {
        match FontSpec::parse(guifont) {
            GuiFont::Spec(spec) => spec,
            other => panic!("解けるはず: {other:?}"),
        }
    }

    #[test]
    fn parses_underscore_as_space() {
        let spec = spec("UDEV_Gothic:h12");
        assert_eq!(spec.families, ["UDEV Gothic"]);
        assert_eq!(spec.size_pt, 12.0);
    }

    #[test]
    fn parses_literal_space_in_family() {
        let spec = spec("MS Gothic:h11");
        assert_eq!(spec.families, ["MS Gothic"]);
        assert_eq!(spec.size_pt, 11.0);
    }

    #[test]
    fn parses_fractional_size() {
        let spec = spec("  Cascadia_Mono:h10.5  ");
        assert_eq!(spec.families, ["Cascadia Mono"]);
        assert_eq!(spec.size_pt, 10.5);
    }

    #[test]
    fn parses_a_candidate_list_and_takes_the_first_size() {
        let spec = spec("UDEV_Gothic:h12,MS_Gothic:h14,Consolas");
        assert_eq!(spec.families, ["UDEV Gothic", "MS Gothic", "Consolas"]);
        assert_eq!(spec.size_pt, 12.0);
    }

    /// nvim 0.12 の組み込み既定値。利用者の指定ではないので警告を出してはいけない。
    #[test]
    fn the_nvim_default_guifont_is_not_a_request() {
        assert_eq!(
            FontSpec::parse("Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace"),
            GuiFont::Unspecified
        );
    }

    #[test]
    fn a_family_without_a_size_leaves_the_font_to_the_gui() {
        assert_eq!(FontSpec::parse(""), GuiFont::Unspecified);
        assert_eq!(FontSpec::parse("   "), GuiFont::Unspecified);
        assert_eq!(FontSpec::parse("MS Gothic"), GuiFont::Unspecified);
    }

    #[test]
    fn rejects_unparsable_specs() {
        // サイズの書き方が壊れている
        assert_eq!(FontSpec::parse("MS Gothic:"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:h"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:habc"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:h0"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:h-12"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:h1200"), GuiFont::Invalid);
        // ファミリ名が無い
        assert_eq!(FontSpec::parse(":h12"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("_:h12"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("A:h12,:h12"), GuiFont::Invalid);
        // 解釈できないオプションが付いている
        assert_eq!(FontSpec::parse("MS Gothic:h12:b"), GuiFont::Invalid);
        assert_eq!(FontSpec::parse("MS Gothic:h12:h14"), GuiFont::Invalid);
    }

    #[test]
    fn default_is_the_bundled_font() {
        let spec = FontSpec::default();
        assert_eq!(spec.families, ["Moralerspace Argon HW"]);
        assert_eq!(spec.size_pt, 12.0);
    }

    #[test]
    fn primary_is_the_first_family_that_exists() {
        let spec = spec("Missing_One:h12,Missing_Two:h12,Consolas:h12");
        assert_eq!(
            spec.primary(|family| family == "Consolas"),
            Some("Consolas")
        );
        assert_eq!(spec.primary(|_| false), None);
    }

    #[test]
    fn fallback_chain_appends_the_bundled_font() {
        assert_eq!(
            spec("UDEV_Gothic:h12").fallback_chain(),
            "UDEV Gothic, Moralerspace Argon HW"
        );
        assert_eq!(
            spec("A:h12,B:h12").fallback_chain(),
            "A, B, Moralerspace Argon HW"
        );
    }

    #[test]
    fn fallback_chain_has_no_duplicate_last_resort() {
        assert_eq!(
            FontSpec::default().fallback_chain(),
            "Moralerspace Argon HW"
        );
        assert_eq!(
            spec("moralerspace_argon_hw:h12").fallback_chain(),
            "moralerspace argon hw"
        );
    }
}

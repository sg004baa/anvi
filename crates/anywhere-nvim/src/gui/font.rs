//! `guifont` の解釈（DESIGN v2 §4.4）。
//!
//! ここは Win32 に一切触らない純粋な文字列処理なので、開発機（Linux）でもテストが回る。
//!
//! 掟はひとつだけ: **解けない指定を既定へ黙って落とさない。** 黙って別のフォントで
//! 描いてしまうと、利用者は `guifont` が効いていないことに気づけない。解けなければ
//! [`FontSpec::parse`] は `None` を返し、呼び出し側が警告を出して現状維持する。

/// CJK を確実に描ける等幅フォント。フォールバック鎖の最後尾であり、既定でもある。
///
/// Windows に標準で入っていて「等幅かつ日本語が全部出る」フォントは実質これだけ
/// （Yu Gothic UI も Meiryo もプロポーショナル）。グリッド UI の土台にはこれを使う。
const LAST_RESORT_FAMILY: &str = "MS Gothic";

/// 既定のフォントサイズ（pt）。
const DEFAULT_SIZE_PT: f32 = 12.0;

/// 受け付けるサイズの下限・上限（pt）。
///
/// 桁を打ち間違えた `guifont`（`:h1200` など）でセル寸法が跳ね上がると、
/// レンダーターゲットの再確保だけで固まったように見える。範囲外は素直に弾く。
const MIN_SIZE_PT: f32 = 1.0;
const MAX_SIZE_PT: f32 = 400.0;

/// 描画に使うフォントの指定。
///
/// `family` は **単一のファミリ名**（DirectWrite に直接渡せる形）で、`guifont` の
/// `_` は空白へ読み替え済み。鎖（プライマリ + 最終手段）が要るときは
/// [`FontSpec::fallback_chain`] を使う。
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub family: String,
    pub size_pt: f32,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self {
            family: LAST_RESORT_FAMILY.to_owned(),
            size_pt: DEFAULT_SIZE_PT,
        }
    }
}

impl FontSpec {
    /// nvim の `guifont`（例 `"UDEV_Gothic:h12"`, `"MS Gothic:h11"`）を解く。
    ///
    /// 受け付ける形は `ファミリ名:h<サイズ>` のみ。
    ///
    /// - `_` は空白として読む（vim 由来の記法）。空白をそのまま含む指定も通る
    ///   （`:set guifont=MS\ Gothic:h11` のバックスラッシュは ex コマンド側で
    ///   消費されるので、オプション値には生の空白が入っている）。
    /// - サイズは必須。無ければ `None`。「既定サイズで描く」という黙った代替はしない。
    /// - `:b` / `:i` のような他のオプションが付いていたら `None`。無視して描くと
    ///   利用者の指定と画面が食い違う。
    /// - カンマ区切りの候補列（`"A:h12,B:h12"`）は `None`。[`FontSpec`] は単一
    ///   ファミリしか表現できず、先頭だけ採るのは残りを黙って捨てることになる。
    #[must_use]
    pub fn parse(guifont: &str) -> Option<Self> {
        let spec = guifont.trim();
        if spec.is_empty() || spec.contains(',') {
            return None;
        }

        let mut parts = spec.split(':');
        let family = parts.next()?.replace('_', " ");
        let family = family.trim();
        if family.is_empty() {
            return None;
        }

        let mut size_pt = None;
        for opt in parts {
            let digits = opt.strip_prefix('h')?;
            if size_pt.is_some() {
                // `:h12:h14` のような矛盾した指定。どちらを採っても嘘になる。
                return None;
            }
            let value: f32 = digits.parse().ok()?;
            if !(MIN_SIZE_PT..=MAX_SIZE_PT).contains(&value) {
                return None;
            }
            size_pt = Some(value);
        }

        Some(Self {
            family: family.to_owned(),
            size_pt: size_pt?,
        })
    }

    /// DirectWrite に渡すファミリ鎖。前から順に「使えるものを使う」。
    ///
    /// 利用者指定 → `MS Gothic` の順に並べ、CJK が確実に出る等幅を最後に置く。
    /// 指定フォントが英数字しか持っていなくても、日本語は最後尾が拾う。
    ///
    /// DirectWrite の `CreateTextFormat` はカンマ区切りのファミリ列を解釈しない
    /// （Silverlight や CSS とは違う）。この文字列はレンダラ側が自分で分解し、
    /// 先頭要素をプライマリとして解決し、残りを `IDWriteFontFallbackBuilder` の
    /// マッピングに積む。区切りの取り決めをここに閉じ込めるための形である。
    #[must_use]
    pub fn fallback_chain(&self) -> String {
        if self.family.eq_ignore_ascii_case(LAST_RESORT_FAMILY) {
            return LAST_RESORT_FAMILY.to_owned();
        }
        format!("{}, {LAST_RESORT_FAMILY}", self.family)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_underscore_as_space() {
        let spec = FontSpec::parse("UDEV_Gothic:h12").expect("解けるはず");
        assert_eq!(spec.family, "UDEV Gothic");
        assert_eq!(spec.size_pt, 12.0);
    }

    #[test]
    fn parses_literal_space_in_family() {
        let spec = FontSpec::parse("MS Gothic:h11").expect("解けるはず");
        assert_eq!(spec.family, "MS Gothic");
        assert_eq!(spec.size_pt, 11.0);
    }

    #[test]
    fn parses_fractional_size() {
        let spec = FontSpec::parse("  Cascadia_Mono:h10.5  ").expect("解けるはず");
        assert_eq!(spec.family, "Cascadia Mono");
        assert_eq!(spec.size_pt, 10.5);
    }

    #[test]
    fn rejects_unparsable_specs() {
        // 空・空白のみ
        assert_eq!(FontSpec::parse(""), None);
        assert_eq!(FontSpec::parse("   "), None);
        // サイズが無い（既定サイズへ黙って落とさない）
        assert_eq!(FontSpec::parse("MS Gothic"), None);
        assert_eq!(FontSpec::parse("MS Gothic:"), None);
        assert_eq!(FontSpec::parse("MS Gothic:h"), None);
        // サイズが数値でない / 範囲外
        assert_eq!(FontSpec::parse("MS Gothic:habc"), None);
        assert_eq!(FontSpec::parse("MS Gothic:h0"), None);
        assert_eq!(FontSpec::parse("MS Gothic:h-12"), None);
        assert_eq!(FontSpec::parse("MS Gothic:h1200"), None);
        // ファミリ名が無い
        assert_eq!(FontSpec::parse(":h12"), None);
        assert_eq!(FontSpec::parse("_:h12"), None);
        // 解釈できないオプションが付いている
        assert_eq!(FontSpec::parse("MS Gothic:h12:b"), None);
        assert_eq!(FontSpec::parse("MS Gothic:h12:h14"), None);
        // カンマ区切りの候補列は表現できない
        assert_eq!(FontSpec::parse("UDEV_Gothic:h12,MS_Gothic:h12"), None);
    }

    #[test]
    fn default_is_the_last_resort_font() {
        let spec = FontSpec::default();
        assert_eq!(spec.family, "MS Gothic");
        assert_eq!(spec.size_pt, 12.0);
    }

    #[test]
    fn fallback_chain_appends_the_last_resort_font() {
        let spec = FontSpec::parse("UDEV_Gothic:h12").expect("解けるはず");
        assert_eq!(spec.fallback_chain(), "UDEV Gothic, MS Gothic");
    }

    #[test]
    fn fallback_chain_has_no_duplicate_last_resort() {
        assert_eq!(FontSpec::default().fallback_chain(), "MS Gothic");
        let spec = FontSpec::parse("ms_gothic:h12").expect("解けるはず");
        assert_eq!(spec.fallback_chain(), "MS Gothic");
    }
}

//! 未確定文字列の状態機械（DESIGN v2 §4.5）。
//!
//! **このアプリの存在理由そのもの。** winit は `GCS_COMPSTR` / `GCS_COMPATTR` を
//! 読んで [`Ime::Preedit`](winit::event::Ime::Preedit) を配り、IME 自身の未確定描画も
//! 抑止してくれる。足りないのは「受け取った未確定文字列を自分で描く」ことだけで、
//! ここが状態を持ち、レンダラがそれを描く。
//!
//! Windows での winit のイベント対応（`platform_impl/windows/event_loop.rs` 実測）:
//!
//! | winit のイベント | Win32 メッセージ |
//! |---|---|
//! | `Ime::Enabled`  | `WM_IME_STARTCOMPOSITION` |
//! | `Ime::Preedit`  | `WM_IME_COMPOSITION` の `GCS_COMPSTR` |
//! | `Ime::Commit`   | `WM_IME_COMPOSITION` / `WM_IME_ENDCOMPOSITION` の `GCS_RESULTSTR` |
//! | `Ime::Disabled` | `WM_IME_ENDCOMPOSITION` |
//!
//! つまり `Enabled` は「IME が使える」ではなく **「composition が始まった」**。
//! [`ImeState::composing`] はこの区間だけ真になり、その間のキーイベントは
//! nvim へ流さない（IME が食ったキーの二重入力を防ぐ）。

use crate::gui::Preedit;

/// 変換中かどうかと、未確定文字列。
#[derive(Debug, Default)]
pub struct ImeState {
    composing: bool,
    preedit: Preedit,
}

impl ImeState {
    /// composition が始まった（`Ime::Enabled`）。
    pub fn begin(&mut self) {
        self.composing = true;
        self.preedit = Preedit::default();
    }

    /// 未確定文字列が更新された（`Ime::Preedit`）。
    ///
    /// `target` は変換対象クラスタのバイト範囲。レンダラはこの範囲で文字列を切って
    /// 反転描画するので、**文字境界に乗っていない範囲はここで捨てる**。壊れた範囲を
    /// そのまま渡すと描画側が panic する。
    pub fn set_preedit(&mut self, text: String, target: Option<(usize, usize)>) {
        let target = target.filter(|&(start, end)| {
            let valid = start <= end
                && end <= text.len()
                && text.is_char_boundary(start)
                && text.is_char_boundary(end);
            if !valid {
                tracing::warn!(start, end, len = text.len(), "ime target range discarded");
            }
            valid
        });
        self.preedit = Preedit { text, target };
    }

    /// 確定した（`Ime::Commit`）。未確定はもう無いが、composition 自体は
    /// `Ime::Disabled` まで続くので [`Self::composing`] は倒さない。
    pub fn commit(&mut self) {
        self.preedit = Preedit::default();
    }

    /// composition が終わった / フォーカスを失った。宙に浮いた未確定を残さない。
    pub fn clear(&mut self) {
        self.composing = false;
        self.preedit = Preedit::default();
    }

    /// 変換中。この間はキーイベントを nvim へ送らない。
    #[must_use]
    pub fn composing(&self) -> bool {
        self.composing
    }

    /// 描くべき未確定文字列。無ければ `None`。
    #[must_use]
    pub fn preedit(&self) -> Option<&Preedit> {
        if self.preedit.is_empty() {
            return None;
        }
        Some(&self.preedit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 確定してから composition が閉じるまでの間もキーを食い止める。
    #[test]
    fn stays_composing_until_disabled() {
        let mut ime = ImeState::default();
        assert!(!ime.composing());

        ime.begin();
        ime.set_preedit("にほん".to_owned(), Some((0, 3)));
        assert!(ime.composing());
        assert_eq!(ime.preedit().map(|p| p.text.as_str()), Some("にほん"));

        ime.commit();
        assert!(ime.composing());
        assert!(ime.preedit().is_none());

        ime.clear();
        assert!(!ime.composing());
    }

    /// winit は取り消しを空文字列の `Preedit` で伝えてくる。
    #[test]
    fn empty_preedit_has_nothing_to_draw() {
        let mut ime = ImeState::default();
        ime.begin();
        ime.set_preedit(String::new(), None);
        assert!(ime.preedit().is_none());
        assert!(ime.composing());
    }

    /// 文字境界に乗っていない範囲はレンダラを壊すので落とす。
    #[test]
    fn rejects_ranges_off_char_boundaries() {
        let mut ime = ImeState::default();
        ime.begin();
        ime.set_preedit("あい".to_owned(), Some((1, 3)));
        assert_eq!(ime.preedit().and_then(|p| p.target), None);

        ime.set_preedit("あい".to_owned(), Some((0, 9)));
        assert_eq!(ime.preedit().and_then(|p| p.target), None);

        ime.set_preedit("あい".to_owned(), Some((3, 6)));
        assert_eq!(ime.preedit().and_then(|p| p.target), Some((3, 6)));
    }
}

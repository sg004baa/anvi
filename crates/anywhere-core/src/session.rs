//! セッション状態機械（DESIGN §6.1）と、保存/破棄の状態契約（DESIGN §4.4）。
//!
//! 契約はコマンド名ではなく状態で決まる。`session_write` を一度でも受け取れば
//! 「最後に書かれた内容」を反映し、一度も受け取らなければ破棄する。
//! `session_end` は反映可否の情報を運ばない。

use tracing::warn;

/// ホスト側のセッション位相（DESIGN §6.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Capturing,
    Editing,
    Applying,
}

/// セッション終了時に何をすべきかの解決結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// 保存済みの内容を対象へ書き戻す。
    WriteBack(Vec<String>),
    /// 保存されたが取得時と同一。書き戻しをスキップする（DESIGN §9.4）。
    Unchanged,
    /// 一度も保存されていない。破棄する。
    Discarded,
}

#[derive(Debug, Default)]
pub struct Session {
    phase: Phase,
    /// 取得時の内容。`Editing` の間だけ意味を持つ。
    original: Vec<String>,
    /// このセッションで最後に受信した `session_write` の内容。
    written: Option<Vec<String>>,
}

impl Session {
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// `Idle` -> `Capturing`。`Idle` 以外では何も変えずに false を返す
    /// （編集中のホットキーは Neovide のフォーカスだけを担当する）。
    pub fn begin_capture(&mut self) -> bool {
        if self.phase != Phase::Idle {
            return false;
        }
        self.phase = Phase::Capturing;
        true
    }

    /// `Capturing` -> `Idle`。取得対象が無かった場合（DESIGN §8.3）。
    pub fn abort_capture(&mut self) {
        if self.phase != Phase::Capturing {
            warn!(phase = ?self.phase, "abort_capture outside Capturing");
            return;
        }
        self.phase = Phase::Idle;
    }

    /// `Capturing` -> `Editing`。取得内容を覚え、`written` を必ずリセットする。
    pub fn begin_edit(&mut self, original: Vec<String>) {
        if self.phase != Phase::Capturing {
            warn!(phase = ?self.phase, "begin_edit outside Capturing");
            return;
        }
        self.original = original;
        self.written = None;
        self.phase = Phase::Editing;
    }

    /// `session_write`: 最後に書かれた内容を覚えるだけ。書き戻しは終了時（DESIGN §4.4 注）。
    pub fn on_write(&mut self, lines: Vec<String>) {
        if self.phase != Phase::Editing {
            warn!(phase = ?self.phase, "session_write outside Editing; ignored");
            return;
        }
        self.written = Some(lines);
    }

    /// `session_end`: `Editing` -> `Applying` -> `Idle`。
    ///
    /// `Editing` 以外での呼び出しは host のバグであり、位相を変えず
    /// 「何もしない」= [`Applied::Discarded`] を返す。
    pub fn on_end(&mut self) -> Applied {
        if self.phase != Phase::Editing {
            warn!(phase = ?self.phase, "session_end outside Editing; ignored");
            return Applied::Discarded;
        }
        self.phase = Phase::Applying;
        let applied = match self.written.take() {
            Some(lines) if lines == self.original => Applied::Unchanged,
            Some(lines) => Applied::WriteBack(lines),
            None => Applied::Discarded,
        };
        self.original.clear();
        self.phase = Phase::Idle;
        applied
    }

    /// 安全網によるリカバリ（DESIGN §6.3）。位相に関係なく Idle へ戻し、
    /// セッションのデータを捨てる。
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.original.clear();
        self.written = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_owned()).collect()
    }

    fn editing(original: &[&str]) -> Session {
        let mut s = Session::default();
        assert!(s.begin_capture());
        s.begin_edit(lines(original));
        assert_eq!(s.phase(), Phase::Editing);
        s
    }

    #[test]
    fn never_written_is_discarded() {
        let mut s = editing(&["a"]);
        assert_eq!(s.on_end(), Applied::Discarded);
        assert_eq!(s.phase(), Phase::Idle);
    }

    #[test]
    fn write_then_end_applies_written() {
        let mut s = editing(&["a"]);
        s.on_write(lines(&["a", "b"]));
        assert_eq!(s.on_end(), Applied::WriteBack(lines(&["a", "b"])));
        assert_eq!(s.phase(), Phase::Idle);
    }

    #[test]
    fn last_write_wins() {
        let mut s = editing(&["a"]);
        s.on_write(lines(&["first"]));
        s.on_write(lines(&["second"]));
        assert_eq!(s.on_end(), Applied::WriteBack(lines(&["second"])));
    }

    #[test]
    fn unchanged_content_is_not_written_back() {
        let mut s = editing(&["a", "b"]);
        s.on_write(lines(&["a", "b"]));
        assert_eq!(s.on_end(), Applied::Unchanged);
    }

    /// 保存後に元の内容へ戻した場合も無変化として扱う（最後の保存だけを見る）。
    #[test]
    fn write_back_to_original_is_unchanged() {
        let mut s = editing(&["a"]);
        s.on_write(lines(&["edited"]));
        s.on_write(lines(&["a"]));
        assert_eq!(s.on_end(), Applied::Unchanged);
    }

    #[test]
    fn begin_capture_rejected_unless_idle() {
        let mut s = Session::default();
        assert!(s.begin_capture());
        assert!(!s.begin_capture());
        assert_eq!(s.phase(), Phase::Capturing);

        s.begin_edit(lines(&["a"]));
        assert!(!s.begin_capture());
        assert_eq!(s.phase(), Phase::Editing);
    }

    #[test]
    fn abort_capture_returns_to_idle() {
        let mut s = Session::default();
        assert!(s.begin_capture());
        s.abort_capture();
        assert_eq!(s.phase(), Phase::Idle);
        assert!(s.begin_capture());
    }

    #[test]
    fn new_session_does_not_inherit_previous_write() {
        let mut s = editing(&["a"]);
        s.on_write(lines(&["edited"]));
        assert_eq!(s.on_end(), Applied::WriteBack(lines(&["edited"])));

        assert!(s.begin_capture());
        s.begin_edit(lines(&["b"]));
        assert_eq!(s.on_end(), Applied::Discarded);
    }

    #[test]
    fn reset_from_any_phase_returns_to_idle() {
        let mut s = Session::default();
        s.reset();
        assert_eq!(s.phase(), Phase::Idle);

        assert!(s.begin_capture());
        s.reset();
        assert_eq!(s.phase(), Phase::Idle);

        let mut s = editing(&["a"]);
        s.on_write(lines(&["edited"]));
        s.reset();
        assert_eq!(s.phase(), Phase::Idle);
        // リセット後のセッションは前回の書き込みを引き継がない
        assert!(s.begin_capture());
        s.begin_edit(lines(&["a"]));
        assert_eq!(s.on_end(), Applied::Discarded);
    }

    #[test]
    fn stray_events_outside_editing_are_ignored() {
        let mut s = Session::default();
        s.on_write(lines(&["x"]));
        assert_eq!(s.phase(), Phase::Idle);
        assert_eq!(s.on_end(), Applied::Discarded);
        assert_eq!(s.phase(), Phase::Idle);

        assert!(s.begin_capture());
        s.on_write(lines(&["x"]));
        assert_eq!(s.phase(), Phase::Capturing);
        assert_eq!(s.on_end(), Applied::Discarded);
        // 位相は保たれる（勝手に Idle へ落とさない）
        assert_eq!(s.phase(), Phase::Capturing);
    }
}

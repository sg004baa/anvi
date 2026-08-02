//! nvim の UI プロトコル（`ext_linegrid`）の状態モデル。
//!
//! ここには **データ型と純粋な参照系だけ** を置く。redraw イベントの適用は
//! [`redraw`](crate::ui::redraw)、キー入力の記法変換は [`input`](crate::ui::input)。
//!
//! 描画側（Windows の Direct2D レンダラ）はこの状態を読むだけで、逆流はしない。

pub mod input;
pub mod redraw;

/// 1 セルが保持できるバイト数。合成文字を含めても実用上これで足りる。
/// 溢れた分は文字境界で切り捨てる（nvim 側も無制限ではない）。
pub const MAX_CELL_BYTES: usize = 16;

/// セルの文字列。ヒープを使わずに `Copy` で持ち回す。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellText {
    buf: [u8; MAX_CELL_BYTES],
    len: u8,
}

impl CellText {
    /// 空白 1 文字。消去済みセルの中身。
    pub const SPACE: Self = {
        let mut buf = [0u8; MAX_CELL_BYTES];
        buf[0] = b' ';
        Self { buf, len: 1 }
    };

    /// `MAX_CELL_BYTES` に収まらない分は文字境界で切り捨てる。
    #[must_use]
    pub fn new(s: &str) -> Self {
        let mut end = s.len().min(MAX_CELL_BYTES);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut buf = [0u8; MAX_CELL_BYTES];
        buf[..end].copy_from_slice(&s.as_bytes()[..end]);
        Self {
            buf,
            len: end as u8,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        // SAFETY 不要: `new` は文字境界でしか切らないので常に妥当な UTF-8。
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    /// **空文字列は「全角の続き」だけを意味する。** 未描画・消去済みのセルは
    /// 空白 1 文字（[`Cell::BLANK`]）であり、ここで真にはならない。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// グリッドの 1 セル。
///
/// 全角文字は「本体セル + 空文字列セル」の 2 セルで表現される（nvim の規約そのまま）。
/// 空文字列セルは直前のセルの続きであり、描画側は読み飛ばす。
///
/// **消去されたセルは空文字列ではなく空白 1 文字**（[`Cell::BLANK`]）。nvim は
/// `grid_clear` や `grid_resize` のあと空白セルを送り直さないので、既定値を空文字列に
/// すると「未描画」と「全角の続き」が区別できなくなり、行末のカーソルが 1 セル
/// 左へずれる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub text: CellText,
    pub hl_id: u32,
}

impl Cell {
    /// 消去済みのセル。既定ハイライトの空白。
    pub const BLANK: Self = Self {
        text: CellText::SPACE,
        hl_id: 0,
    };
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}

/// 文字グリッド。`grid_resize` の度に張り直す。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
}

impl Grid {
    /// 全セルが空白の新しいグリッド。
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::BLANK; cols * rows],
        }
    }

    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// 行のセル列。範囲外の行は空スライス。
    #[must_use]
    pub fn row(&self, row: usize) -> &[Cell] {
        if row >= self.rows {
            return &[];
        }
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    /// 行の可変スライス。範囲外の行は空スライス。
    pub fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        if row >= self.rows {
            return &mut [];
        }
        let start = row * self.cols;
        let end = start + self.cols;
        &mut self.cells[start..end]
    }

    /// 全セルを空白へ戻す。nvim はこの後で空白セルを送り直さない。
    pub fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
    }

    /// `src` 行の `[left, right)` を `dst` 行の同じ列へ写す。`grid_scroll` 専用。
    ///
    /// 行を作り直さず `copy_within` で動かすのは、スクロールが毎フレーム走るため。
    /// グリッドの外へはみ出す指定は切り詰める（呼び出し側が範囲を検証済み）。
    pub fn copy_row_range(&mut self, src: usize, dst: usize, left: usize, right: usize) {
        if src == dst || src >= self.rows || dst >= self.rows {
            return;
        }
        let right = right.min(self.cols);
        if left >= right {
            return;
        }
        let from = src * self.cols + left;
        self.cells
            .copy_within(from..from + (right - left), dst * self.cols + left);
    }

    /// 行のテキスト（全角の続きセルは飛ばす）。テストと検証用。
    /// 行末までの空白がそのまま入るので、比較する側で `trim_end` すること。
    #[must_use]
    pub fn row_text(&self, row: usize) -> String {
        let mut out = String::new();
        for cell in self.row(row) {
            if cell.text.is_empty() {
                continue;
            }
            out.push_str(cell.text.as_str());
        }
        out
    }
}

/// 24bit カラー。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    #[must_use]
    pub fn channels(self) -> (u8, u8, u8) {
        (
            (self.0 >> 16) as u8,
            ((self.0 >> 8) & 0xff) as u8,
            (self.0 & 0xff) as u8,
        )
    }
}

/// 下線の種類。`hl_attr_define` の underline 系フラグを 1 つに畳んだもの。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Line,
    Undercurl,
    Double,
    Dotted,
    Dashed,
}

/// `hl_attr_define` の rgb 属性。未指定は `None` のまま持ち、既定色との合成は
/// [`Highlights::style`] で行う。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HlAttr {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub sp: Option<Rgb>,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: Underline,
    /// 0..=100。未指定は 0。
    pub blend: u8,
}

/// 描画に必要な最終色。`reverse` と既定色の解決は済んでいる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub sp: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: Underline,
}

/// ハイライト表。id は密なので `Vec` で持つ。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlights {
    default_fg: Rgb,
    default_bg: Rgb,
    default_sp: Rgb,
    attrs: Vec<Option<HlAttr>>,
}

impl Highlights {
    #[must_use]
    pub fn default_fg(&self) -> Rgb {
        self.default_fg
    }

    #[must_use]
    pub fn default_bg(&self) -> Rgb {
        self.default_bg
    }

    #[must_use]
    pub fn default_sp(&self) -> Rgb {
        self.default_sp
    }

    pub fn set_defaults(&mut self, fg: Rgb, bg: Rgb, sp: Rgb) {
        self.default_fg = fg;
        self.default_bg = bg;
        self.default_sp = sp;
    }

    pub fn define(&mut self, id: u32, attr: HlAttr) {
        let id = id as usize;
        if id >= self.attrs.len() {
            self.attrs.resize(id + 1, None);
        }
        self.attrs[id] = Some(attr);
    }

    #[must_use]
    pub fn attr(&self, id: u32) -> Option<&HlAttr> {
        self.attrs.get(id as usize).and_then(Option::as_ref)
    }

    /// 既定色と `reverse` を解決した最終スタイル。未知の id は既定色。
    #[must_use]
    pub fn style(&self, id: u32) -> Style {
        let attr = self.attr(id).copied().unwrap_or_default();
        let fg = attr.fg.unwrap_or(self.default_fg);
        let bg = attr.bg.unwrap_or(self.default_bg);
        let (fg, bg) = if attr.reverse { (bg, fg) } else { (fg, bg) };
        Style {
            fg,
            bg,
            sp: attr.sp.unwrap_or(self.default_sp),
            bold: attr.bold,
            italic: attr.italic,
            strikethrough: attr.strikethrough,
            underline: attr.underline,
        }
    }
}

/// カーソルのセル座標。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorPos {
    pub row: usize,
    pub col: usize,
}

/// `mode_info_set` のカーソル形状。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Horizontal,
    Vertical,
}

/// `mode_info_set` の 1 モード分。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeInfo {
    pub name: String,
    pub shape: CursorShape,
    /// 縦棒 / 横棒の太さ（セルに対する %）。ブロックでは無意味。
    pub cell_percentage: u8,
    /// カーソルの色に使うハイライト id。0 は「セル色の反転」。
    pub attr_id: u32,
}

/// 現在のモードと `mode_info_set` の表。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeState {
    modes: Vec<ModeInfo>,
    current: usize,
    name: String,
    cursor_style_enabled: bool,
}

impl ModeState {
    pub fn set_modes(&mut self, modes: Vec<ModeInfo>, cursor_style_enabled: bool) {
        self.modes = modes;
        self.cursor_style_enabled = cursor_style_enabled;
        if self.current >= self.modes.len() {
            self.current = 0;
        }
    }

    pub fn set_current(&mut self, name: String, index: usize) {
        self.name = name;
        self.current = index;
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn cursor_style_enabled(&self) -> bool {
        self.cursor_style_enabled
    }

    /// 現在モードの情報。`mode_info_set` がまだ来ていなければ `None`。
    #[must_use]
    pub fn info(&self) -> Option<&ModeInfo> {
        self.modes.get(self.current)
    }

    /// このモードで文字を打ち込めるか。IME の有効・無効はこれで切り替える。
    ///
    /// nvim が送るモード名（`mode_change` の第 1 引数）で判定する。挿入・置換・
    /// コマンドライン・端末・選択の各モードだけが文字入力を受け付ける。
    #[must_use]
    pub fn accepts_text_input(&self) -> bool {
        matches!(
            self.name.split('_').next().unwrap_or_default(),
            "insert" | "replace" | "cmdline" | "terminal" | "select"
        )
    }
}

/// UI の全状態。`redraw` バッチを適用する先。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiState {
    pub grid: Grid,
    pub hl: Highlights,
    pub cursor: CursorPos,
    pub mode: ModeState,
    pub title: String,
    pub busy: bool,
    /// `option_set guifont`。未設定なら `None`。
    pub guifont: Option<String>,
}

/// `redraw` バッチを 1 回適用した結果。描画側が「何をし直すべきか」を知るための印。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RedrawOutcome {
    /// `flush` が含まれていた。フレームの区切りであり、ここで描画する。
    pub flushed: bool,
    /// `grid_resize` が来た。
    pub resized: bool,
    /// `mode_change` が来た。IME の有効・無効を見直す。
    pub mode_changed: bool,
    /// `option_set guifont` が来た。フォントを作り直す。
    pub font_changed: bool,
    /// `set_title` が来た。
    pub title_changed: bool,
}

//! Direct2D / DirectWrite による自前描画（DESIGN v2 §4.4）。
//!
//! このファイルの本題は [`Renderer::paint_preedit`] であり、グリッド描画は
//! その土台に過ぎない。UI を自前で持つ理由が「未確定文字列（preedit）をインラインで
//! 出す」ことだからである。preedit は **カーソル位置から 1 本の [`IDWriteTextLayout`]
//! として描き、変換対象クラスタだけを反転させる**。ここが動かなければアプリの
//! 存在意義が無いので、他のどこよりも丁寧に書いてある。
//!
//! 座標系の掟がひとつある。**すべて物理ピクセルで計算する。**
//! Direct2D は DIP（1/96 インチ）で座標を解釈するので、レンダーターゲットの DPI を
//! `SetDpi(96, 96)` に固定して「1 DIP = 1 物理ピクセル」にし、`scale`（winit の
//! `scale_factor`）はフォントサイズを決めるときに一度だけ掛ける。こうすると
//! 「D2D が勝手に掛ける倍率」と「自分が掛ける倍率」が二重になる事故が起きない。
//!
//! セル幅を整数へ丸めていないのも意図的である。1 行を 1 本のテキストレイアウトで
//! 描く以上、`col * cell_width` は DirectWrite が積み上げるグリフの送り幅と
//! 一致していなければならない。丸めると 90 桁目で数十ピクセルずれる。
//! 一方セル高さは行ごとに独立なので、行境界がぼやけないよう整数へ切り上げる。

use std::collections::HashMap;

use anyhow::{Context as _, bail};
use anywhere_core::ui::{CursorShape, ModeInfo, Rgb, Style, UiState, Underline};
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, ERROR_INSUFFICIENT_BUFFER, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_BOLD,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_METRICS, DWRITE_HIT_TEST_METRICS,
    DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_UNICODE_RANGE,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory2, IDWriteFontCollection,
    IDWriteTextFormat, IDWriteTextFormat1, IDWriteTextLayout,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{BOOL, HRESULT, Interface as _, PCWSTR};
use windows_numerics::Vector2;

use crate::focus::as_hwnd;
use crate::gui::Preedit;
use crate::gui::font::FontSpec;

/// レイアウト全体を指すテキスト範囲。
const WHOLE: DWRITE_TEXT_RANGE = DWRITE_TEXT_RANGE {
    startPosition: 0,
    length: u32::MAX,
};

/// 「バッファが足りない」を表す HRESULT。
///
/// `IDWriteTextLayout::HitTestTextRange` は件数問い合わせでこれを返す。**正常応答**
/// なのでこの 1 つだけは通し、他のエラーはすべて上へ返す。
const E_NOT_SUFFICIENT_BUFFER: HRESULT = HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0);

/// セル寸法を決める基準グリフ。等幅フォントなら全 ASCII が同じ送り幅を持つ。
const REFERENCE_GLYPH: char = 'M';

/// 1 セルの寸法（物理ピクセル）。
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
}

/// 下線・取り消し線の位置と太さ（物理ピクセル、セル上端からの相対）。
///
/// DirectWrite の `SetUnderline` に頼らないのは、レイアウトごとに引かれる線が
/// セル境界で途切れて見えるため。位置だけフォントから借りて、線は自分で塗る。
#[derive(Clone, Copy, Debug)]
struct Decor {
    underline_top: f32,
    underline_thickness: f32,
    strike_top: f32,
    strike_thickness: f32,
    /// 論理 1 ピクセルに相当する物理ピクセル数。preedit の下線の基準。
    hairline: f32,
}

/// 自前で引く横線 1 本（物理ピクセル）。下線・取り消し線・破線が共有する。
#[derive(Clone, Copy, Debug)]
struct Line {
    left: f32,
    right: f32,
    top: f32,
    thickness: f32,
}

impl Line {
    fn rect(self) -> D2D_RECT_F {
        rect_of(self.left, self.top, self.right, self.top + self.thickness)
    }
}

/// フォントを 1 つ解決した結果。[`Renderer::new`] と [`Renderer::set_font`] が共有する。
struct Font {
    format: IDWriteTextFormat,
    cell: CellMetrics,
    decor: Decor,
}

/// グリッドを HWND へ描くレンダラ。
///
/// COM オブジェクトの解放は `Drop` に任せる（`windows` の COM 型は参照カウントを持つ）。
pub struct Renderer {
    target: ID2D1HwndRenderTarget,
    dwrite: IDWriteFactory2,
    format: IDWriteTextFormat,
    cell: CellMetrics,
    decor: Decor,
    /// レンダーターゲットの物理ピクセルサイズ。preedit の右端打ち切りに使う。
    size: (u32, u32),
    /// 色ごとのブラシ。`CreateSolidColorBrush` を毎フレーム回すと D2D 内部で
    /// リソースが作られ続けるので、色をキーに使い回す。
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
}

impl Renderer {
    /// HWND に紐づいたレンダーターゲットを作る。
    ///
    /// `scale` は winit の `scale_factor`。DPI 変更やフォント変更のたびに
    /// [`Renderer::set_font`] を呼び直す前提なので、ここでは初期値として使うだけ。
    pub fn new(hwnd: isize, font: &FontSpec, scale: f64) -> anyhow::Result<Self> {
        let hwnd = as_hwnd(hwnd);
        let size = client_size(hwnd)?;

        // SAFETY: 引数は列挙値とオプション（None）だけ。生成に失敗すれば Err が返る。
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }
                .context("D2D1CreateFactory failed")?;

        let props = D2D1_RENDER_TARGET_PROPERTIES::default();
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: size.0,
                height: size.1,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        // SAFETY: 両方のプロパティはこのスタックフレームに生きている値への参照で、
        // 呼び出し中だけ読まれる。hwnd は GetClientRect が通った実在ウィンドウ。
        let target = unsafe { factory.CreateHwndRenderTarget(&props, &hwnd_props) }
            .context("CreateHwndRenderTarget failed")?;
        // ここで factory を手放してよい。D2D のリソースは生成元 factory を参照
        // カウントで保持するので、ターゲットが生きている間 factory も生き続ける。
        drop(factory);

        // SAFETY: 生成直後のターゲットへの設定。D2D の DIP と物理ピクセルを 1:1 に
        // 固定し、倍率は自分で掛ける（モジュール冒頭の掟）。
        unsafe { target.SetDpi(96.0, 96.0) };
        // SAFETY: 同上。矩形をピクセル境界へ吸着させ、セル境界に継ぎ目を出さない。
        // 文字のアンチエイリアスは別設定（既定の ClearType）なので影響しない。
        unsafe { target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED) };

        // SAFETY: 引数は列挙値のみ。共有ファクトリなのでプロセス内で再利用される。
        // `IDWriteFactory2` を要求するのは `CreateFontFallbackBuilder` が要るため。
        let dwrite: IDWriteFactory2 = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
            .context("DWriteCreateFactory failed")?;

        let resolved = resolve_font(&dwrite, font, scale)?;

        Ok(Self {
            target,
            dwrite,
            format: resolved.format,
            cell: resolved.cell,
            decor: resolved.decor,
            size,
            brushes: HashMap::new(),
        })
    }

    #[must_use]
    pub fn metrics(&self) -> CellMetrics {
        self.cell
    }

    /// フォントを差し替える。`guifont` の変更と DPI 変更の両方がここへ来る。
    ///
    /// 失敗したときは **何も変えない**（解決を先にやってから代入する）。
    /// 呼び出し側が警告を出して現状維持できるようにするため。
    pub fn set_font(&mut self, font: &FontSpec, scale: f64) -> anyhow::Result<()> {
        let resolved = resolve_font(&self.dwrite, font, scale)?;
        self.format = resolved.format;
        self.cell = resolved.cell;
        self.decor = resolved.decor;
        Ok(())
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32) -> anyhow::Result<()> {
        let size = D2D_SIZE_U {
            width: width_px,
            height: height_px,
        };
        // SAFETY: size はこのフレームに生きている値。ターゲットは有効な COM 参照。
        unsafe { self.target.Resize(&size) }.context("failed to resize the Direct2D target")?;
        self.size = (width_px, height_px);
        Ok(())
    }

    /// 1 フレーム描く。
    ///
    /// `EndDraw` が `D2DERR_RECREATE_TARGET` を返したら握り潰さず `Err` にする。
    /// デバイスが失われたときに黙って描き続けると画面が固まったまま気づけない。
    /// 呼び出し側が [`Renderer::new`] からやり直すのが正しい復帰手順である。
    pub fn draw(&mut self, ui: &UiState, preedit: Option<&Preedit>) -> anyhow::Result<()> {
        // SAFETY: 以降の描画呼び出しはすべて BeginDraw と EndDraw の間にある。
        unsafe { self.target.BeginDraw() };
        let painted = self.paint(ui, preedit);
        // SAFETY: 直前の BeginDraw と対。paint が途中で失敗しても必ず閉じる
        // （閉じないと次フレームの BeginDraw が D2DERR_WRONG_STATE で落ちる）。
        let ended = unsafe { self.target.EndDraw(None, None) };

        painted?;
        match ended {
            Ok(()) => Ok(()),
            Err(err) if err.code() == D2DERR_RECREATE_TARGET => {
                bail!("the Direct2D render target was lost and must be recreated")
            }
            Err(err) => Err(err).context("EndDraw failed"),
        }
    }

    /// `BeginDraw` と `EndDraw` の間の中身。
    ///
    /// 順序に意味がある。背景 → 前景 → カーソル → preedit。preedit は
    /// 「いま入力している文字」なので必ず最前面に来る。
    fn paint(&mut self, ui: &UiState, preedit: Option<&Preedit>) -> anyhow::Result<()> {
        let clear = color_f(ui.hl.default_bg());
        // SAFETY: BeginDraw 済み。clear はこのフレームに生きている値。
        unsafe { self.target.Clear(Some(&clear)) };

        self.paint_backgrounds(ui)?;
        self.paint_foregrounds(ui)?;
        self.paint_cursor(ui)?;
        if let Some(preedit) = preedit.filter(|p| !p.is_empty()) {
            self.paint_preedit(ui, preedit)?;
        }
        Ok(())
    }

    /// 行ごとに、同じ背景色が続く区間をまとめて塗る。
    ///
    /// 既定色の区間は `Clear` が済ませているので飛ばす。画面の大半は既定色なので、
    /// これだけで塗り潰しの回数が桁で減る。
    fn paint_backgrounds(&mut self, ui: &UiState) -> anyhow::Result<()> {
        let default_bg = ui.hl.default_bg();
        let cols = ui.grid.cols();
        for row in 0..ui.grid.rows() {
            let cells = ui.grid.row(row);
            let mut col = 0;
            while col < cols {
                let bg = ui.hl.style(cells[col].hl_id).bg;
                let start = col;
                col += 1;
                while col < cols && ui.hl.style(cells[col].hl_id).bg == bg {
                    col += 1;
                }
                if bg == default_bg {
                    continue;
                }
                let brush = self.brush(bg)?;
                let cell = self.cell;
                self.fill(
                    rect_of(
                        start as f32 * cell.width,
                        row as f32 * cell.height,
                        col as f32 * cell.width,
                        (row + 1) as f32 * cell.height,
                    ),
                    &brush,
                );
            }
        }
        Ok(())
    }

    /// 行ごとに前景ランを描く。
    ///
    /// ランの切れ目は「スタイルが変わる」「全角セル」「空文字列セル（全角の続き）」。
    /// 全角は 1 セル 1 ランで単独に描く。フォールバックで異幅のグリフが選ばれたとき、
    /// 後続の文字までずれてしまうのを 1 セルに封じ込めるため。
    fn paint_foregrounds(&mut self, ui: &UiState) -> anyhow::Result<()> {
        let cols = ui.grid.cols();
        let mut buf: Vec<u16> = Vec::with_capacity(cols.saturating_mul(2));
        for row in 0..ui.grid.rows() {
            let cells = ui.grid.row(row);
            let mut col = 0;
            while col < cols {
                let cell = cells[col];
                if cell.text.is_empty() {
                    // 全角の続き。本体セルのランが 2 セル分を描いている。
                    col += 1;
                    continue;
                }
                let style = ui.hl.style(cell.hl_id);
                let wide = col + 1 < cols && cells[col + 1].text.is_empty();

                buf.clear();
                buf.extend(cell.text.as_str().encode_utf16());
                let mut span = if wide { 2 } else { 1 };
                if !wide {
                    while col + span < cols {
                        let next = cells[col + span];
                        if next.text.is_empty() {
                            break;
                        }
                        // 次のセルが全角なら、そこから新しいランを始める。
                        if col + span + 1 < cols && cells[col + span + 1].text.is_empty() {
                            break;
                        }
                        if ui.hl.style(next.hl_id) != style {
                            break;
                        }
                        buf.extend(next.text.as_str().encode_utf16());
                        span += 1;
                    }
                }

                self.paint_run(row, col, span, &buf, &style)?;
                col += span;
            }
        }
        Ok(())
    }

    /// 1 ランを描く。装飾（下線・取り消し線）を先に、文字を後に。
    fn paint_run(
        &mut self,
        row: usize,
        col: usize,
        span: usize,
        text: &[u16],
        style: &Style,
    ) -> anyhow::Result<()> {
        let cell = self.cell;
        let x = col as f32 * cell.width;
        let y = row as f32 * cell.height;
        let width = span as f32 * cell.width;

        if style.underline != Underline::None {
            let brush = self.brush(style.sp)?;
            self.paint_underline(style.underline, x, x + width, y, &brush);
        }
        if style.strikethrough {
            // 取り消し線は文字と同じ色で引く（nvim も special ではなく前景色を使う）。
            let brush = self.brush(style.fg)?;
            let line = Line {
                left: x,
                right: x + width,
                top: y + self.decor.strike_top,
                thickness: self.decor.strike_thickness,
            };
            self.fill(line.rect(), &brush);
        }

        // 空白だけのランはグリフを持たない。画面の大半がこれなので、
        // レイアウト生成そのものを省く（装飾はもう引いてある）。
        if text.iter().all(|&unit| unit == u16::from(b' ')) {
            return Ok(());
        }

        let brush = self.brush(style.fg)?;
        let layout = self.styled_layout(text, style, width, cell.height)?;
        // SAFETY: BeginDraw 済み。layout と brush はこのフレームで生きている。
        unsafe {
            self.target.DrawTextLayout(
                Vector2::new(x, y),
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        Ok(())
    }

    /// 下線を自前で引く。`y` はセル上端。
    fn paint_underline(
        &self,
        kind: Underline,
        left: f32,
        right: f32,
        y: f32,
        brush: &ID2D1SolidColorBrush,
    ) {
        let line = Line {
            left,
            right,
            top: y + self.decor.underline_top,
            thickness: self.decor.underline_thickness,
        };
        match kind {
            Underline::None => {}
            Underline::Line => self.fill(line.rect(), brush),
            Underline::Double => {
                self.fill(line.rect(), brush);
                // 2 本目はセルの底を超えない位置に置く。
                let second = Line {
                    top: (line.top + line.thickness * 2.0)
                        .min(y + self.cell.height - line.thickness),
                    ..line
                };
                self.fill(second.rect(), brush);
            }
            // 波線は諦めて点線で代用する。1 セル数ピクセルの高さに収まる波は、
            // ベジエを引いても点線と見分けが付かないうえ、セル境界で位相が合わず
            // 継ぎ目が目立つ。undercurl と dotted を同じ見た目にするのは割り切り。
            Underline::Undercurl | Underline::Dotted => {
                self.paint_dashes(line, line.thickness, line.thickness, brush);
            }
            Underline::Dashed => {
                self.paint_dashes(line, line.thickness * 3.0, line.thickness * 2.0, brush);
            }
        }
    }

    /// 破線・点線。`on` が線の長さ、`off` が隙間。
    fn paint_dashes(&self, line: Line, on: f32, off: f32, brush: &ID2D1SolidColorBrush) {
        let on = on.max(1.0);
        let step = (on + off).max(1.0);
        let mut x = line.left;
        while x < line.right {
            let dash = Line {
                left: x,
                right: (x + on).min(line.right),
                ..line
            };
            self.fill(dash.rect(), brush);
            x += step;
        }
    }

    /// カーソル。
    ///
    /// `busy_start` の間は描かない（nvim は「カーソルを消せ」の意味で送ってくる）。
    /// `cursor_style_enabled` が偽なら `mode_info_set` の形は使わずブロックにする。
    fn paint_cursor(&mut self, ui: &UiState) -> anyhow::Result<()> {
        if ui.busy {
            return Ok(());
        }
        let cols = ui.grid.cols();
        if ui.cursor.row >= ui.grid.rows() || ui.cursor.col >= cols {
            return Ok(());
        }
        let row = ui.cursor.row;
        let cells = ui.grid.row(row);

        // カーソルが全角の続きセルに乗ったら本体セルへ寄せる。
        let col = if cells[ui.cursor.col].text.is_empty() && ui.cursor.col > 0 {
            ui.cursor.col - 1
        } else {
            ui.cursor.col
        };
        let cell = cells[col];
        let wide = col + 1 < cols && cells[col + 1].text.is_empty();

        let fallback = ModeInfo::default();
        let info = ui.mode.info().unwrap_or(&fallback);
        let styled = ui.mode.cursor_style_enabled();
        let shape = if styled {
            info.shape
        } else {
            CursorShape::Block
        };

        // attr_id が 0 なら「セル色の反転」。それ以外はそのハイライトの色で描き、
        // 文字はその前景色で塗り直す。
        let cell_style = ui.hl.style(cell.hl_id);
        let (fill, ink) = if styled && info.attr_id != 0 {
            let style = ui.hl.style(info.attr_id);
            (style.bg, style.fg)
        } else {
            (cell_style.fg, cell_style.bg)
        };

        let metrics = self.cell;
        let x = col as f32 * metrics.width;
        let y = row as f32 * metrics.height;
        let width = if wide {
            metrics.width * 2.0
        } else {
            metrics.width
        };
        let percentage = if info.cell_percentage == 0 {
            // 形だけ来て太さが無い指定を「見えないカーソル」にはしない。
            100.0
        } else {
            f32::from(info.cell_percentage.min(100)) / 100.0
        };

        let fill_brush = self.brush(fill)?;
        match shape {
            CursorShape::Block => {
                self.fill(rect_of(x, y, x + width, y + metrics.height), &fill_brush);
                let text = cell.text.as_str();
                if !text.is_empty() && text != " " {
                    let buf: Vec<u16> = text.encode_utf16().collect();
                    let brush = self.brush(ink)?;
                    let layout = self.styled_layout(&buf, &cell_style, width, metrics.height)?;
                    // SAFETY: BeginDraw 済み。layout と brush はこのフレームで生きている。
                    unsafe {
                        self.target.DrawTextLayout(
                            Vector2::new(x, y),
                            &layout,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                        );
                    }
                }
            }
            CursorShape::Vertical => {
                let bar = (width * percentage).max(1.0);
                self.fill(rect_of(x, y, x + bar, y + metrics.height), &fill_brush);
            }
            CursorShape::Horizontal => {
                let bar = (metrics.height * percentage).max(1.0);
                let top = y + metrics.height - bar;
                self.fill(rect_of(x, top, x + width, y + metrics.height), &fill_brush);
            }
        }
        Ok(())
    }

    /// 未確定文字列のオーバーレイ。**このアプリの存在理由。**
    ///
    /// 見た目の取り決め:
    ///
    /// - カーソルセルの左上を原点に、preedit 全体を **1 本のレイアウト**として描く。
    ///   セルへ切り分けないのは、IME が返す文字列にグリッド幅の概念が無いためで、
    ///   クラスタ境界とセル境界を無理に合わせると変換対象の範囲がずれる。
    /// - 下地を既定 bg で塗ってから既定 fg で描く。下のグリッド文字と重なって
    ///   読めなくなるのを防ぐ。
    /// - 全体に細い下線（論理 1px）。「まだ確定していない」の合図。
    /// - `target`（変換対象クラスタ）だけ fg/bg を反転し、太い下線（論理 2px）を足す。
    ///   反転した区間では「地」が既定 fg なので、下線は既定 bg で引く。そうしないと
    ///   同色になって消える。
    /// - 行末を越える分は右端で打ち切る。折り返しは v2 ではやらない
    ///   （折り返すとカーソル行が下へずれ、下のグリッドと二重に見える）。
    ///
    /// 失敗しうる処理（レイアウト生成・ヒットテスト・ブラシ生成）は **クリップを
    /// 積む前に**すべて済ませる。`PushAxisAlignedClip` と `PopAxisAlignedClip` の間で
    /// 早期 return すると `EndDraw` が状態エラーで落ちるため。
    fn paint_preedit(&mut self, ui: &UiState, preedit: &Preedit) -> anyhow::Result<()> {
        let metrics = self.cell;
        let cols = ui.grid.cols();
        let rows = ui.grid.rows();
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        let col = ui.cursor.col.min(cols - 1);
        let row = ui.cursor.row.min(rows - 1);
        let x = col as f32 * metrics.width;
        let y = row as f32 * metrics.height;

        // 右端はグリッドの終わりかウィンドウの終わり、狭いほう。
        let right = (cols as f32 * metrics.width).min(self.size.0 as f32);
        let available = right - x;
        if available <= 0.0 {
            return Ok(());
        }

        let text: Vec<u16> = preedit.text.encode_utf16().collect();
        let layout = self.layout(&text, available, metrics.height)?;
        let mut text_metrics = DWRITE_TEXT_METRICS::default();
        // SAFETY: 直前に作ったレイアウトへの問い合わせ。出力先はローカル変数。
        unsafe { layout.GetMetrics(&mut text_metrics) }.context("GetMetrics failed")?;
        let width = text_metrics
            .widthIncludingTrailingWhitespace
            .min(available)
            .max(1.0);

        let targets = match preedit.target {
            Some((start, end)) if end > start => {
                hit_test_range(&layout, &preedit.text, start, end, x, y)?
            }
            _ => Vec::new(),
        };

        let ink = self.brush(ui.hl.default_fg())?;
        let under = self.brush(ui.hl.default_bg())?;

        let clip = rect_of(x, y, x + available, y + metrics.height);
        let body = rect_of(x, y, x + width, y + metrics.height);
        let thin_top = y + self.decor.underline_top;
        let thin = rect_of(x, thin_top, x + width, thin_top + self.decor.hairline);
        let thick_top = (thin_top - self.decor.hairline).max(y);
        let thick_bottom = (thick_top + self.decor.hairline * 2.0).min(y + metrics.height);
        let origin = Vector2::new(x, y);

        // SAFETY: BeginDraw 済み。ここから Pop までは戻り値を持たない D2D 呼び出し
        // だけを並べ、クリップの積み降ろしが必ず対称になるようにしている。
        unsafe {
            self.target
                .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
            self.target.FillRectangle(&body, &under);
            self.target
                .DrawTextLayout(origin, &layout, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE);
            self.target.FillRectangle(&thin, &ink);

            for &(left, range_right) in &targets {
                let cover = rect_of(left, y, range_right, y + metrics.height);
                self.target.FillRectangle(&cover, &ink);
                // 反転色で描き直すのは、この区間のグリフだけ。クリップで縛れば
                // レイアウトを分割せずに部分反転できる。
                self.target
                    .PushAxisAlignedClip(&cover, D2D1_ANTIALIAS_MODE_ALIASED);
                self.target
                    .DrawTextLayout(origin, &layout, &under, D2D1_DRAW_TEXT_OPTIONS_NONE);
                self.target.PopAxisAlignedClip();
                let bar = rect_of(left, thick_top, range_right, thick_bottom);
                self.target.FillRectangle(&bar, &under);
            }

            self.target.PopAxisAlignedClip();
        }
        Ok(())
    }

    /// 現在のフォントで 1 ランぶんのレイアウトを作る。
    ///
    /// 折り返しは書式側で無効にしてあるので、`max_width` は描画を切らない。
    fn layout(
        &self,
        text: &[u16],
        max_width: f32,
        max_height: f32,
    ) -> anyhow::Result<IDWriteTextLayout> {
        // SAFETY: text は呼び出し中だけ読まれる生存スライス。format は有効な COM 参照。
        unsafe {
            self.dwrite
                .CreateTextLayout(text, &self.format, max_width, max_height)
        }
        .context("CreateTextLayout failed")
    }

    /// 太字・斜体を適用したレイアウト。
    ///
    /// 合成太字は送り幅を変えうるので、ラン単位でしか使わない
    /// （行を丸ごと 1 本のレイアウトにして太字を混ぜると桁がずれる）。
    fn styled_layout(
        &self,
        text: &[u16],
        style: &Style,
        max_width: f32,
        max_height: f32,
    ) -> anyhow::Result<IDWriteTextLayout> {
        let layout = self.layout(text, max_width, max_height)?;
        if style.bold {
            // SAFETY: 直前に作ったレイアウトへの設定。範囲は全体。
            unsafe { layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, WHOLE) }
                .context("SetFontWeight failed")?;
        }
        if style.italic {
            // SAFETY: 同上。
            unsafe { layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, WHOLE) }
                .context("SetFontStyle failed")?;
        }
        Ok(layout)
    }

    /// 色のブラシ。無ければ作って覚える。
    ///
    /// 返すのはクローン（COM の `AddRef` 1 回）。`CreateSolidColorBrush` の再生成に
    /// 比べれば桁違いに安く、呼び出し側が複数のブラシを同時に持てるようになる。
    fn brush(&mut self, color: Rgb) -> anyhow::Result<ID2D1SolidColorBrush> {
        if let Some(brush) = self.brushes.get(&color.0) {
            return Ok(brush.clone());
        }
        let value = color_f(color);
        // SAFETY: value はこのフレームに生きている値。ブラシはターゲットが所有する。
        let brush = unsafe { self.target.CreateSolidColorBrush(&value, None) }
            .context("CreateSolidColorBrush failed")?;
        self.brushes.insert(color.0, brush.clone());
        Ok(brush)
    }

    fn fill(&self, rect: D2D_RECT_F, brush: &ID2D1SolidColorBrush) {
        // SAFETY: BeginDraw と EndDraw の間でのみ呼ばれる。rect はローカル値、
        // brush は呼び出し中生きている参照。
        unsafe { self.target.FillRectangle(&rect, brush) };
    }
}

/// フォントを解決してセル寸法と書式を作る。
///
/// `px_size = size_pt * scale * 96 / 72`。pt からピクセルへの換算をここ 1 か所に閉じる。
fn resolve_font(dwrite: &IDWriteFactory2, font: &FontSpec, scale: f64) -> anyhow::Result<Font> {
    let scale = scale as f32;
    if !(scale.is_finite() && scale > 0.0) {
        bail!("invalid scale factor {scale}");
    }
    let px_size = font.size_pt * scale * 96.0 / 72.0;
    if !(px_size.is_finite() && px_size > 0.0) {
        bail!("invalid font size {}pt at scale {scale}", font.size_pt);
    }

    let (cell, decor) = measure(dwrite, &font.family, px_size, scale)?;
    let format = make_format(dwrite, font, px_size, cell)?;
    Ok(Font {
        format,
        cell,
        decor,
    })
}

/// primary family の実体からセル寸法と装飾位置を割り出す。
///
/// フォントが入っていなければ既定へ落とさず `Err`。黙って別のフォントで描くと、
/// `guifont` が効いていないことに利用者が気づけない（`font.rs` と同じ掟）。
fn measure(
    dwrite: &IDWriteFactory2,
    family: &str,
    px_size: f32,
    scale: f32,
) -> anyhow::Result<(CellMetrics, Decor)> {
    let mut collection: Option<IDWriteFontCollection> = None;
    // SAFETY: 出力先はローカル変数。checkforupdates は false（毎回の再走査は不要）。
    unsafe { dwrite.GetSystemFontCollection(&mut collection, false) }
        .context("GetSystemFontCollection failed")?;
    let collection = collection.context("the system font collection is unavailable")?;

    let name = wide_nul(family);
    let mut index = 0u32;
    let mut exists = BOOL(0);
    // SAFETY: name は NUL 終端の生存スライス。index と exists はローカル変数。
    unsafe { collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists) }
        .context("FindFamilyName failed")?;
    if !exists.as_bool() {
        bail!("font family {family:?} is not installed");
    }

    // SAFETY: index は FindFamilyName が返した実在するファミリの添字。
    let font_family = unsafe { collection.GetFontFamily(index) }.context("GetFontFamily failed")?;
    // SAFETY: 太さ・幅・斜体は列挙値。等幅の基準寸法は regular から採る。
    let face_source = unsafe {
        font_family.GetFirstMatchingFont(
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
        )
    }
    .context("GetFirstMatchingFont failed")?;
    // SAFETY: 直前に得たフォントからフェイスを作るだけ。
    let face = unsafe { face_source.CreateFontFace() }.context("CreateFontFace failed")?;

    let mut font_metrics = DWRITE_FONT_METRICS::default();
    // SAFETY: 出力先はローカル変数。失敗しない問い合わせ（戻り値なし）。
    unsafe { face.GetMetrics(&mut font_metrics) };
    let design_units = f32::from(font_metrics.designUnitsPerEm);
    if design_units <= 0.0 {
        bail!("font family {family:?} reports designUnitsPerEm = 0");
    }
    let unit = px_size / design_units;

    let codepoint = [u32::from(REFERENCE_GLYPH)];
    let mut glyph = [0u16; 1];
    // SAFETY: 入力・出力ともに長さ 1 のローカル配列で、渡す個数も 1。
    unsafe { face.GetGlyphIndices(codepoint.as_ptr(), 1, glyph.as_mut_ptr()) }
        .context("GetGlyphIndices failed")?;
    if glyph[0] == 0 {
        bail!("font family {family:?} has no glyph for {REFERENCE_GLYPH:?}");
    }
    let mut glyph_metrics = [DWRITE_GLYPH_METRICS::default(); 1];
    // SAFETY: 同上。issideways は false（横組みのみ扱う）。
    unsafe { face.GetDesignGlyphMetrics(glyph.as_ptr(), 1, glyph_metrics.as_mut_ptr(), false) }
        .context("GetDesignGlyphMetrics failed")?;

    // 幅は丸めない（モジュール冒頭の理由）。高さは行境界を締めるため切り上げる。
    let width = glyph_metrics[0].advanceWidth as f32 * unit;
    if !(width.is_finite() && width > 0.0) {
        bail!("font family {family:?} reports a zero advance width");
    }
    let raw_height = (f32::from(font_metrics.ascent)
        + f32::from(font_metrics.descent)
        + f32::from(font_metrics.lineGap))
        * unit;
    let height = raw_height.ceil().max(1.0);
    let ascent = (f32::from(font_metrics.ascent) * unit)
        .round()
        .clamp(0.0, height);

    let underline_thickness = (f32::from(font_metrics.underlineThickness) * unit).max(1.0);
    // underlinePosition はベースラインからの符号付きオフセット（下方向が負）。
    let underline_top = (ascent - f32::from(font_metrics.underlinePosition) * unit)
        .clamp(0.0, (height - underline_thickness).max(0.0));
    let strike_thickness = (f32::from(font_metrics.strikethroughThickness) * unit).max(1.0);
    let strike_top = (ascent - f32::from(font_metrics.strikethroughPosition) * unit)
        .clamp(0.0, (height - strike_thickness).max(0.0));

    Ok((
        CellMetrics {
            width,
            height,
            ascent,
        },
        Decor {
            underline_top,
            underline_thickness,
            strike_top,
            strike_thickness,
            hairline: scale.max(1.0),
        },
    ))
}

/// テキスト書式。行送りをセル高さに固定し、ベースラインを ascent に固定する。
///
/// こうしておけば `DrawTextLayout` の原点にセルの左上をそのまま渡せる
/// （レイアウトが勝手にベースラインを決めない）。
fn make_format(
    dwrite: &IDWriteFactory2,
    font: &FontSpec,
    px_size: f32,
    cell: CellMetrics,
) -> anyhow::Result<IDWriteTextFormat> {
    let family = wide_nul(&font.family);
    // ロケールは空文字列＝システム既定。日本語環境では ja-JP が使われ、
    // 漢字の字形が中国語圏の異体字にならない。ここで決め打ちにはしない。
    let locale = wide_nul("");
    // SAFETY: family と locale は NUL 終端の生存スライス。コレクションは None
    // （システムフォント）。残りは列挙値と数値。
    let format = unsafe {
        dwrite.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None::<&IDWriteFontCollection>,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            px_size,
            PCWSTR(locale.as_ptr()),
        )
    }
    .context("CreateTextFormat failed")?;

    // SAFETY: 直前に作った書式への設定。すべて列挙値と数値。
    unsafe {
        format
            .SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)
            .context("SetWordWrapping failed")?;
        format
            .SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)
            .context("SetTextAlignment failed")?;
        format
            .SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)
            .context("SetParagraphAlignment failed")?;
        format
            .SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, cell.height, cell.ascent)
            .context("SetLineSpacing failed")?;
    }

    install_fallback(dwrite, &format, font)?;
    Ok(format)
}

/// `FontSpec::fallback_chain` を DirectWrite のフォントフォールバックに積む。
///
/// `CreateTextFormat` はカンマ区切りのファミリ列を解釈しないので、鎖はここで分解する。
/// 全 Unicode を鎖（利用者指定 → `MS Gothic`）へ写す 1 本のマッピングを置き、その後に
/// システム既定のフォールバックを足す。**鎖の先頭に primary を入れてある**ので、
/// 「フォールバックが基底フォントより先に引かれるか」という DirectWrite の仕様の
/// 曖昧さに結果が左右されない。指定フォントが持つ文字は必ず指定フォントで出る。
fn install_fallback(
    dwrite: &IDWriteFactory2,
    format: &IDWriteTextFormat,
    font: &FontSpec,
) -> anyhow::Result<()> {
    let chain = font.fallback_chain();
    let names: Vec<Vec<u16>> = chain
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(wide_nul)
        .collect();
    if names.is_empty() {
        bail!("the font fallback chain for {:?} is empty", font.family);
    }
    let pointers: Vec<*const u16> = names.iter().map(|name| name.as_ptr()).collect();
    let ranges = [DWRITE_UNICODE_RANGE {
        first: 0,
        last: 0x0010_FFFF,
    }];

    // SAFETY: builder は直後に使い切るローカル。ranges と pointers（およびその
    // 指す names）は AddMapping の呼び出し中ずっと生きている。
    let fallback = unsafe {
        let builder = dwrite
            .CreateFontFallbackBuilder()
            .context("CreateFontFallbackBuilder failed")?;
        builder
            .AddMapping(
                &ranges,
                &pointers,
                None::<&IDWriteFontCollection>,
                PCWSTR::null(),
                PCWSTR::null(),
                1.0,
            )
            .context("AddMapping failed")?;
        let system = dwrite
            .GetSystemFontFallback()
            .context("GetSystemFontFallback failed")?;
        builder.AddMappings(&system).context("AddMappings failed")?;
        builder
            .CreateFontFallback()
            .context("CreateFontFallback failed")?
    };

    let format1: IDWriteTextFormat1 = format
        .cast()
        .context("IDWriteTextFormat1 is unavailable on this system")?;
    // SAFETY: format1 は同一オブジェクトの別インターフェース。fallback は有効な参照。
    unsafe { format1.SetFontFallback(&fallback) }.context("SetFontFallback failed")?;
    Ok(())
}

/// 変換対象クラスタの水平範囲（左, 右）を取る。
///
/// `target` は preedit 文字列の **バイト** 範囲なので、DirectWrite が数える
/// UTF-16 コードユニットへ換算してから渡す。ここを間違えると、サロゲートペアや
/// 全角の混じった文字列で反転位置が右へずれていく。
fn hit_test_range(
    layout: &IDWriteTextLayout,
    text: &str,
    start: usize,
    end: usize,
    origin_x: f32,
    origin_y: f32,
) -> anyhow::Result<Vec<(f32, f32)>> {
    let position = utf16_len_upto(text, start);
    let length = utf16_len_upto(text, end).saturating_sub(position);
    if length == 0 {
        return Ok(Vec::new());
    }

    let mut count = 0u32;
    // 単一行・双方向なしなら 1 個で足りる。実測が超えたときだけヒープへ逃げる。
    let mut inline = [DWRITE_HIT_TEST_METRICS::default(); 4];
    // SAFETY: 出力先はローカル配列と変数。件数不足なら count に必要数が入る。
    let probe = unsafe {
        layout.HitTestTextRange(
            position,
            length,
            origin_x,
            origin_y,
            Some(&mut inline),
            &mut count,
        )
    };
    match probe {
        Ok(()) => {
            let used = (count as usize).min(inline.len());
            Ok(collect_ranges(&inline[..used]))
        }
        Err(err) if err.code() == E_NOT_SUFFICIENT_BUFFER => {
            let mut heap = vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
            // SAFETY: heap は count 個ぶん確保済み。出力先はローカル。
            unsafe {
                layout.HitTestTextRange(
                    position,
                    length,
                    origin_x,
                    origin_y,
                    Some(&mut heap),
                    &mut count,
                )
            }
            .context("HitTestTextRange failed")?;
            let used = (count as usize).min(heap.len());
            Ok(collect_ranges(&heap[..used]))
        }
        Err(err) => Err(err).context("HitTestTextRange failed"),
    }
}

fn collect_ranges(metrics: &[DWRITE_HIT_TEST_METRICS]) -> Vec<(f32, f32)> {
    metrics
        .iter()
        .filter(|metric| metric.width > 0.0)
        .map(|metric| (metric.left, metric.left + metric.width))
        .collect()
}

/// バイト位置までの UTF-16 コードユニット数。
///
/// 境界の内側を指されたときは、その文字を丸ごと手前に数える（範囲が単調に増える
/// ほうが、反転が 1 クラスタ広いより破綻しない）。
fn utf16_len_upto(text: &str, byte: usize) -> u32 {
    let mut units = 0u32;
    for (offset, ch) in text.char_indices() {
        if offset >= byte {
            break;
        }
        units += ch.len_utf16() as u32;
    }
    units
}

/// Win32 に渡す NUL 終端の UTF-16 列。
fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

const fn rect_of(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn color_f(color: Rgb) -> D2D1_COLOR_F {
    let (r, g, b) = color.channels();
    D2D1_COLOR_F {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
}

/// クライアント領域の物理ピクセルサイズ。
///
/// レンダーターゲットの初期サイズに使う。0 を許すのは、まだ表示していない
/// ウィンドウ（`with_visible(false)`）でも生成できるようにするため。
fn client_size(hwnd: HWND) -> anyhow::Result<(u32, u32)> {
    let mut rect = RECT::default();
    // SAFETY: hwnd は winit が作った実在ウィンドウ。出力先はローカル変数。
    unsafe { GetClientRect(hwnd, &mut rect) }.context("GetClientRect failed")?;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::{utf16_len_upto, wide_nul};

    #[test]
    fn utf16_offsets_follow_byte_offsets() {
        // ASCII はバイトとコードユニットが 1:1。
        assert_eq!(utf16_len_upto("hello", 0), 0);
        assert_eq!(utf16_len_upto("hello", 3), 3);
        assert_eq!(utf16_len_upto("hello", 5), 5);
    }

    #[test]
    fn utf16_offsets_count_bmp_and_astral_chars() {
        // 「あい」は 3 バイト / 1 コードユニットずつ。
        assert_eq!(utf16_len_upto("あい", 3), 1);
        assert_eq!(utf16_len_upto("あい", 6), 2);
        // サロゲートペアは 4 バイト / 2 コードユニット。ここを 1 と数えると
        // 変換対象の反転位置が右へずれていく。
        assert_eq!(utf16_len_upto("𠀋あ", 4), 2);
        assert_eq!(utf16_len_upto("𠀋あ", 7), 3);
    }

    #[test]
    fn utf16_offsets_clamp_outside_the_string() {
        assert_eq!(utf16_len_upto("あ", 99), 1);
        assert_eq!(utf16_len_upto("", 4), 0);
    }

    #[test]
    fn utf16_offsets_round_mid_char_bytes_up() {
        // 境界の内側（「あ」の 2 バイト目）は、その文字を手前に数える。
        assert_eq!(utf16_len_upto("あい", 1), 1);
    }

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(wide_nul("Mi"), vec![0x4d, 0x69, 0]);
        assert_eq!(wide_nul(""), vec![0]);
    }
}

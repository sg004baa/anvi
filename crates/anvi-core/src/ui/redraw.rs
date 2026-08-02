//! `redraw` 通知（`ext_linegrid`）を [`UiState`] へ適用する。
//!
//! nvim は UI クライアントに「画面をこう描け」という命令列を投げてくる。ここはその
//! **唯一の**解釈器で、描画側は出来上がった [`UiState`] を読むだけにする。パースを
//! 描画ループへ散らすと、フレームを落とした瞬間に状態が壊れて二度と復旧しないため。
//!
//! 方針（→ v2 計画 §4.1）:
//!
//! - `ui_attach` で立てるのは `rgb` と `ext_linegrid` だけなので、グリッドは 1 番しか
//!   来ない。それ以外の grid のイベントは debug ログを出して捨てる。
//! - **未知のイベント名は無視する。** UI プロトコルは拡張されうるので、知らない名前で
//!   落ちるのは実害しかない。これは silent fallback ではなく仕様。
//! - **既知イベントの引数が壊れていたら `Err`。** 形が変わったことに気付かないまま
//!   画面だけ崩れるのが最悪なので、`unwrap_or` で埋め合わせない。取り出しは下の
//!   小さなヘルパに閉じ込め、「何を読もうとして失敗したか」を必ずエラーに残す。
//! - スクロールで空いた領域は消さない。nvim は直後の `grid_line` が埋める前提で差分を
//!   送るので、ここでクリアすると nvim の想定とずれる。

use anyhow::{Result, anyhow, bail};
use rmpv::Value;
use tracing::debug;

use crate::ui::{
    Cell, CellText, CursorPos, CursorShape, Grid, HlAttr, ModeInfo, RedrawOutcome, Rgb, UiState,
    Underline,
};

/// 扱うグリッド。`ext_multigrid` を要求しないので nvim はこれしか作らない。
const GRID: u64 = 1;

/// `redraw` 通知の params をそのまま適用する。
///
/// batch の各要素は `[event_name, args_1, args_2, ...]`。1 つのイベント名に対して
/// 引数タプルが複数入る（nvim が同種のイベントをまとめて送るため）ので、タプルの数だけ
/// 適用する。引数を取らないイベントは空タプル 1 つで来るが、タプルが 1 つも無い
/// `[event_name]` も「1 回発火した」と読む。
///
/// # Errors
///
/// 既知イベントの引数が期待した形でない場合。未知のイベント名はエラーにしない。
pub fn apply(state: &mut UiState, batch: &[Value]) -> Result<RedrawOutcome> {
    let mut outcome = RedrawOutcome::default();
    for event in batch {
        let parts = as_array(event, "redraw event")?;
        let (name, calls) = parts
            .split_first()
            .ok_or_else(|| anyhow!("redraw event: the event array is empty"))?;
        let name = as_str(name, "redraw event name")?;
        if calls.is_empty() {
            apply_one(state, &mut outcome, name, &[])?;
            continue;
        }
        for call in calls {
            let args = as_array(call, name)?;
            apply_one(state, &mut outcome, name, args)?;
        }
    }
    Ok(outcome)
}

fn apply_one(
    state: &mut UiState,
    outcome: &mut RedrawOutcome,
    event: &str,
    args: &[Value],
) -> Result<()> {
    match event {
        "grid_resize" => {
            if !targets_our_grid(args, event)? {
                return Ok(());
            }
            let cols = as_usize(arg(args, 1, event)?, "grid_resize width")?;
            let rows = as_usize(arg(args, 2, event)?, "grid_resize height")?;
            state.grid = Grid::new(cols, rows);
            outcome.resized = true;
        }
        "grid_clear" => {
            if !targets_our_grid(args, event)? {
                return Ok(());
            }
            state.grid.clear();
        }
        "grid_destroy" => {
            if !targets_our_grid(args, event)? {
                return Ok(());
            }
            // グリッド 1 が壊されることは通常ない。来たなら描くものが無いという意味
            // なので、空グリッドにして描画側に何も出させない。
            state.grid = Grid::default();
        }
        "grid_cursor_goto" => {
            if !targets_our_grid(args, event)? {
                return Ok(());
            }
            let row = as_usize(arg(args, 1, event)?, "grid_cursor_goto row")?;
            let col = as_usize(arg(args, 2, event)?, "grid_cursor_goto col")?;
            state.cursor = CursorPos { row, col };
        }
        "grid_line" => grid_line(state, args)?,
        "grid_scroll" => grid_scroll(state, args)?,
        "hl_attr_define" => hl_attr_define(state, args)?,
        "default_colors_set" => {
            let fg = as_rgb(arg(args, 0, event)?, "default_colors_set rgb_fg")?;
            let bg = as_rgb(arg(args, 1, event)?, "default_colors_set rgb_bg")?;
            let sp = as_rgb(arg(args, 2, event)?, "default_colors_set rgb_sp")?;
            // cterm_fg / cterm_bg は 256 色端末用。GUI では読まない。
            state.hl.set_defaults(fg, bg, sp);
        }
        "mode_info_set" => mode_info_set(state, args)?,
        "mode_change" => {
            let name = as_str(arg(args, 0, event)?, "mode_change mode_name")?.to_owned();
            let index = as_usize(arg(args, 1, event)?, "mode_change mode_idx")?;
            state.mode.set_current(name, index);
            outcome.mode_changed = true;
        }
        "option_set" => {
            let option = as_str(arg(args, 0, event)?, "option_set name")?;
            let value = arg(args, 1, event)?;
            if option == "guifont" {
                let spec = as_str(value, "option_set guifont")?;
                // nvim は未設定を空文字列で送る。`None` が「未設定」なので詰め替える。
                state.guifont = (!spec.is_empty()).then(|| spec.to_owned());
                outcome.font_changed = true;
            } else {
                debug!(option, "unhandled ui option");
            }
        }
        "set_title" => {
            state.title = as_str(arg(args, 0, event)?, "set_title title")?.to_owned();
            outcome.title_changed = true;
        }
        "busy_start" => state.busy = true,
        "busy_stop" => state.busy = false,
        "flush" => outcome.flushed = true,
        _ => debug!(event, "unhandled redraw event"),
    }
    Ok(())
}

fn grid_line(state: &mut UiState, args: &[Value]) -> Result<()> {
    const EVENT: &str = "grid_line";
    if !targets_our_grid(args, EVENT)? {
        return Ok(());
    }
    let row = as_usize(arg(args, 1, EVENT)?, "grid_line row")?;
    let col_start = as_usize(arg(args, 2, EVENT)?, "grid_line col_start")?;
    let cells = as_array(arg(args, 3, EVENT)?, "grid_line cells")?;
    // `wrap` は自前で折り返さないので使わないが、形だけは検証する。引数が 1 つ増減
    // したことに気付けないまま列がずれるのを防ぐため。
    as_bool(arg(args, 4, EVENT)?, "grid_line wrap")?;

    let rows = state.grid.rows();
    if row >= rows {
        bail!("grid_line: row {row} is outside the {rows}-row grid");
    }
    let target = state.grid.row_mut(row);
    let cols = target.len();

    // hl_id は「同じ `grid_line` 呼び出し内の直近」を継ぐ。バッチや呼び出しを跨いで
    // 引き継ぐと、別の行の色が飛び火する。最初のセルには必ず hl_id が付く
    // （`:h ui-event-grid_line`）ので、無いまま省略されたら契約違反として落とす。
    let mut inherited: Option<u32> = None;
    let mut col = col_start;
    for cell in cells {
        let parts = as_array(cell, "grid_line cell")?;
        if parts.is_empty() || parts.len() > 3 {
            bail!(
                "grid_line: a cell must be [text(, hl_id, repeat)], got {} items",
                parts.len()
            );
        }
        let text = as_str(&parts[0], "grid_line cell text")?;
        if let Some(value) = parts.get(1) {
            inherited = Some(as_u32(value, "grid_line cell hl_id")?);
        }
        let hl_id =
            inherited.ok_or_else(|| anyhow!("grid_line: the first cell carries no hl_id"))?;
        let repeat = match parts.get(2) {
            Some(value) => as_usize(value, "grid_line cell repeat")?,
            None => 1,
        };

        // 全角の右半分は空文字列セルとして来る。そのまま格納し、描画側が
        // 「直前セルの続き」として読み飛ばす。
        let cell = Cell {
            text: CellText::new(text),
            hl_id,
        };
        for _ in 0..repeat {
            let slot = target
                .get_mut(col)
                .ok_or_else(|| anyhow!("grid_line: column {col} is outside the {cols} columns"))?;
            *slot = cell;
            col += 1;
        }
    }
    Ok(())
}

fn grid_scroll(state: &mut UiState, args: &[Value]) -> Result<()> {
    const EVENT: &str = "grid_scroll";
    if !targets_our_grid(args, EVENT)? {
        return Ok(());
    }
    let top = as_usize(arg(args, 1, EVENT)?, "grid_scroll top")?;
    let bot = as_usize(arg(args, 2, EVENT)?, "grid_scroll bot")?;
    let left = as_usize(arg(args, 3, EVENT)?, "grid_scroll left")?;
    let right = as_usize(arg(args, 4, EVENT)?, "grid_scroll right")?;
    let rows = as_i64(arg(args, 5, EVENT)?, "grid_scroll rows")?;
    let cols = as_i64(arg(args, 6, EVENT)?, "grid_scroll cols")?;
    if cols != 0 {
        bail!("grid_scroll: horizontal scrolling is not in the protocol (cols={cols})");
    }

    let (grid_rows, grid_cols) = (state.grid.rows(), state.grid.cols());
    if top > bot || bot > grid_rows {
        bail!("grid_scroll: row range [{top}, {bot}) does not fit a {grid_rows}-row grid");
    }
    if left > right || right > grid_cols {
        bail!("grid_scroll: column range [{left}, {right}) does not fit {grid_cols} columns");
    }

    let distance = as_usize_from(rows.unsigned_abs(), "grid_scroll rows")?;
    // 領域より遠くへ動かすなら写す行は 1 つも残らない。空いた領域を消さないのが
    // 契約なので、ここは本当に何もしないのが正しい（直後の `grid_line` が埋める）。
    if distance == 0 || distance >= bot - top {
        return Ok(());
    }

    if rows > 0 {
        // 上へ。先頭から写せば元データを踏まない。
        for dst in top..bot - distance {
            state.grid.copy_row_range(dst + distance, dst, left, right);
        }
    } else {
        // 下へ。末尾から写さないと、これから読む行を先に潰す。
        for dst in (top + distance..bot).rev() {
            state.grid.copy_row_range(dst - distance, dst, left, right);
        }
    }
    Ok(())
}

fn hl_attr_define(state: &mut UiState, args: &[Value]) -> Result<()> {
    const EVENT: &str = "hl_attr_define";
    let id = as_u32(arg(args, 0, EVENT)?, "hl_attr_define id")?;
    // cterm_attr / info は GUI では使わない。読むのは rgb 側だけ。
    let rgb = as_map(arg(args, 1, EVENT)?, "hl_attr_define rgb_attr")?;

    let mut attr = HlAttr::default();
    // underline 系は複数立ちうる。マップの並び順に依存しないよう、いったん全部
    // 受けてから固定した優先順で 1 つに畳む。
    let (mut line, mut curl, mut double, mut dotted, mut dashed) =
        (false, false, false, false, false);
    for (key, value) in rgb {
        let key = as_str(key, "hl_attr_define attribute name")?;
        match key {
            "foreground" => attr.fg = Some(as_rgb(value, key)?),
            "background" => attr.bg = Some(as_rgb(value, key)?),
            "special" => attr.sp = Some(as_rgb(value, key)?),
            "reverse" => attr.reverse = as_bool(value, key)?,
            "bold" => attr.bold = as_bool(value, key)?,
            "italic" => attr.italic = as_bool(value, key)?,
            "strikethrough" => attr.strikethrough = as_bool(value, key)?,
            "underline" => line = as_bool(value, key)?,
            "undercurl" => curl = as_bool(value, key)?,
            "underdouble" => double = as_bool(value, key)?,
            "underdotted" => dotted = as_bool(value, key)?,
            "underdashed" => dashed = as_bool(value, key)?,
            "blend" => {
                let blend = as_u8(value, key)?;
                if blend > 100 {
                    bail!("hl_attr_define: blend must be 0..=100, got {blend}");
                }
                attr.blend = blend;
            }
            other => debug!(attribute = other, "unhandled hl attribute"),
        }
    }
    attr.underline = if line {
        Underline::Line
    } else if curl {
        Underline::Undercurl
    } else if double {
        Underline::Double
    } else if dotted {
        Underline::Dotted
    } else if dashed {
        Underline::Dashed
    } else {
        Underline::None
    };

    state.hl.define(id, attr);
    Ok(())
}

fn mode_info_set(state: &mut UiState, args: &[Value]) -> Result<()> {
    const EVENT: &str = "mode_info_set";
    let enabled = as_bool(arg(args, 0, EVENT)?, "mode_info_set cursor_style_enabled")?;
    let list = as_array(arg(args, 1, EVENT)?, "mode_info_set mode_info list")?;

    let mut modes = Vec::with_capacity(list.len());
    for entry in list {
        let map = as_map(entry, "mode_info_set mode_info")?;
        // 無いキーは既定値のまま。`HlAttr` と同じ扱い。
        let mut info = ModeInfo::default();
        for (key, value) in map {
            let key = as_str(key, "mode_info_set mode_info key")?;
            match key {
                "name" => info.name = as_str(value, "mode_info name")?.to_owned(),
                "cursor_shape" => {
                    info.shape = cursor_shape(as_str(value, "mode_info cursor_shape")?)?;
                }
                "cell_percentage" => info.cell_percentage = as_u8(value, key)?,
                "attr_id" => info.attr_id = as_u32(value, key)?,
                other => debug!(key = other, "unhandled mode_info key"),
            }
        }
        modes.push(info);
    }
    state.mode.set_modes(modes, enabled);
    Ok(())
}

fn cursor_shape(name: &str) -> Result<CursorShape> {
    match name {
        "block" => Ok(CursorShape::Block),
        "horizontal" => Ok(CursorShape::Horizontal),
        "vertical" => Ok(CursorShape::Vertical),
        other => bail!("mode_info_set: unknown cursor_shape {other:?}"),
    }
}

/// grid 引数を読み、追跡対象なら `true`。1 番以外は捨てる。
fn targets_our_grid(args: &[Value], event: &str) -> Result<bool> {
    let grid = as_u64(arg(args, 0, event)?, "grid id")?;
    if grid != GRID {
        debug!(event, grid, "dropped an event for an untracked grid");
        return Ok(false);
    }
    Ok(true)
}

fn arg<'a>(args: &'a [Value], index: usize, event: &str) -> Result<&'a Value> {
    args.get(index).ok_or_else(|| {
        anyhow!(
            "{event}: argument {index} is missing (the event carries {})",
            args.len()
        )
    })
}

fn as_array<'a>(value: &'a Value, what: &str) -> Result<&'a [Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("{what}: expected an array, got {value:?}"))
}

fn as_map<'a>(value: &'a Value, what: &str) -> Result<&'a [(Value, Value)]> {
    value
        .as_map()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("{what}: expected a map, got {value:?}"))
}

fn as_str<'a>(value: &'a Value, what: &str) -> Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| anyhow!("{what}: expected a string, got {value:?}"))
}

fn as_bool(value: &Value, what: &str) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| anyhow!("{what}: expected a boolean, got {value:?}"))
}

fn as_u64(value: &Value, what: &str) -> Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| anyhow!("{what}: expected a non-negative integer, got {value:?}"))
}

fn as_i64(value: &Value, what: &str) -> Result<i64> {
    value
        .as_i64()
        .ok_or_else(|| anyhow!("{what}: expected an integer, got {value:?}"))
}

fn as_u32(value: &Value, what: &str) -> Result<u32> {
    let raw = as_u64(value, what)?;
    u32::try_from(raw).map_err(|_| anyhow!("{what}: {raw} does not fit in u32"))
}

/// 24bit カラー。`ext_termcolors` を立てないので nvim は常に妥当な色を送ってくる
/// （`:h ui-linegrid`）。-1 や 24bit を超える値は契約違反なので落とす。
fn as_rgb(value: &Value, what: &str) -> Result<Rgb> {
    let raw = as_u32(value, what)?;
    if raw > 0x00ff_ffff {
        bail!("{what}: {raw} is not a 24-bit color");
    }
    Ok(Rgb(raw))
}

fn as_u8(value: &Value, what: &str) -> Result<u8> {
    let raw = as_u64(value, what)?;
    u8::try_from(raw).map_err(|_| anyhow!("{what}: {raw} does not fit in u8"))
}

fn as_usize(value: &Value, what: &str) -> Result<usize> {
    as_usize_from(as_u64(value, what)?, what)
}

fn as_usize_from(raw: u64, what: &str) -> Result<usize> {
    usize::try_from(raw).map_err(|_| anyhow!("{what}: {raw} does not fit in usize"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::from(s)
    }

    /// `[event_name, args_1, args_2, ...]` を組む。
    fn event(name: &str, calls: Vec<Vec<Value>>) -> Value {
        let mut parts = vec![Value::from(name)];
        parts.extend(calls.into_iter().map(Value::Array));
        Value::Array(parts)
    }

    fn cell(text: &str, hl_id: Option<u32>, repeat: Option<u64>) -> Value {
        let mut parts = vec![Value::from(text)];
        if let Some(hl_id) = hl_id {
            parts.push(Value::from(hl_id));
        }
        if let Some(repeat) = repeat {
            parts.push(Value::from(repeat));
        }
        Value::Array(parts)
    }

    fn line(row: u64, col: u64, cells: Vec<Value>) -> Value {
        event(
            "grid_line",
            vec![vec![
                Value::from(1u64),
                Value::from(row),
                Value::from(col),
                Value::Array(cells),
                Value::from(false),
            ]],
        )
    }

    fn sized(cols: u64, rows: u64) -> UiState {
        let mut state = UiState::default();
        apply(
            &mut state,
            &[event(
                "grid_resize",
                vec![vec![
                    Value::from(1u64),
                    Value::from(cols),
                    Value::from(rows),
                ]],
            )],
        )
        .expect("grid_resize");
        state
    }

    fn hl_ids(state: &UiState, row: usize) -> Vec<u32> {
        state.grid.row(row).iter().map(|c| c.hl_id).collect()
    }

    fn texts(state: &UiState, row: usize) -> Vec<&str> {
        state
            .grid
            .row(row)
            .iter()
            .map(|c| c.text.as_str())
            .collect()
    }

    /// 行を 1 文字で塗る。`grid_scroll` のテスト用。
    fn paint(state: &mut UiState, row: u64, glyph: &str) {
        let cols = state.grid.cols() as u64;
        apply(
            state,
            &[line(row, 0, vec![cell(glyph, Some(0), Some(cols))])],
        )
        .expect("paint a row");
    }

    #[test]
    fn grid_line_inherits_the_hl_id_of_the_previous_cell_in_the_same_call() {
        let mut state = sized(4, 1);
        apply(
            &mut state,
            &[line(
                0,
                0,
                vec![
                    cell("a", Some(7), None),
                    cell("b", None, None),
                    cell("c", Some(9), None),
                    cell("d", None, None),
                ],
            )],
        )
        .expect("apply");
        assert_eq!(hl_ids(&state, 0), vec![7, 7, 9, 9]);
    }

    /// 継承は呼び出しの中だけ。跨いで引き継ぐ実装だと、この 2 本目が 9 を拾って
    /// 素通りしてしまう。
    #[test]
    fn grid_line_does_not_inherit_the_hl_id_across_calls() {
        let mut state = sized(4, 1);
        apply(&mut state, &[line(0, 0, vec![cell("a", Some(9), None)])]).expect("apply");
        let err = apply(&mut state, &[line(0, 1, vec![cell("b", None, None)])])
            .expect_err("a call whose first cell has no hl_id is a protocol violation");
        assert!(
            err.to_string().contains("first cell"),
            "unexpected error: {err:#}"
        );
    }

    /// `repeat` は「その回数だけ置く」であって「追加で置く」ではない。
    #[test]
    fn grid_line_repeat_counts_the_first_placement() {
        let mut state = sized(6, 1);
        apply(
            &mut state,
            &[line(
                0,
                1,
                vec![cell("x", Some(1), Some(3)), cell("y", None, None)],
            )],
        )
        .expect("apply");
        assert_eq!(texts(&state, 0), vec!["", "x", "x", "x", "y", ""]);
    }

    #[test]
    fn grid_line_keeps_the_empty_continuation_cell_of_a_wide_char() {
        let mut state = sized(4, 1);
        apply(
            &mut state,
            &[line(
                0,
                0,
                vec![
                    cell("あ", Some(0), None),
                    cell("", None, None),
                    cell("い", None, None),
                    cell("", None, None),
                ],
            )],
        )
        .expect("apply");
        assert_eq!(texts(&state, 0), vec!["あ", "", "い", ""]);
        assert_eq!(state.grid.row_text(0), "あい");
    }

    #[test]
    fn grid_line_rejects_a_run_that_overflows_the_row() {
        let mut state = sized(3, 1);
        let err = apply(&mut state, &[line(0, 1, vec![cell("x", Some(0), Some(3))])])
            .expect_err("a run past the last column must not be silently clipped");
        assert!(err.to_string().contains("column 3"), "unexpected: {err:#}");
    }

    /// `rows > 0` は上へ。列は半開区間 `[left, right)` で、外は触らない。
    /// 空いた末尾は **消さない**。
    #[test]
    fn grid_scroll_moves_rows_up_within_the_column_range() {
        let mut state = sized(3, 5);
        for (row, glyph) in ["0", "1", "2", "3", "4"].iter().enumerate() {
            paint(&mut state, row as u64, glyph);
        }
        apply(
            &mut state,
            &[event(
                "grid_scroll",
                vec![vec![
                    Value::from(1u64),
                    Value::from(0u64),
                    Value::from(5u64),
                    Value::from(1u64),
                    Value::from(3u64),
                    Value::from(2i64),
                    Value::from(0i64),
                ]],
            )],
        )
        .expect("apply");

        assert_eq!(texts(&state, 0), vec!["0", "2", "2"]);
        assert_eq!(texts(&state, 1), vec!["1", "3", "3"]);
        assert_eq!(texts(&state, 2), vec!["2", "4", "4"]);
        // 空いた分はそのまま残す（直後の grid_line が埋める）。
        assert_eq!(texts(&state, 3), vec!["3", "3", "3"]);
        assert_eq!(texts(&state, 4), vec!["4", "4", "4"]);
    }

    /// `rows < 0` は下へ。末尾から写さないと、これから読む行を先に潰す。
    #[test]
    fn grid_scroll_moves_rows_down_without_smearing() {
        let mut state = sized(2, 5);
        for (row, glyph) in ["0", "1", "2", "3", "4"].iter().enumerate() {
            paint(&mut state, row as u64, glyph);
        }
        apply(
            &mut state,
            &[event(
                "grid_scroll",
                vec![vec![
                    Value::from(1u64),
                    Value::from(0u64),
                    Value::from(5u64),
                    Value::from(0u64),
                    Value::from(2u64),
                    Value::from(-2i64),
                    Value::from(0i64),
                ]],
            )],
        )
        .expect("apply");

        let rows: Vec<String> = (0..5).map(|r| state.grid.row_text(r)).collect();
        assert_eq!(rows, vec!["00", "11", "00", "11", "22"]);
    }

    #[test]
    fn grid_scroll_rejects_horizontal_scrolling() {
        let mut state = sized(2, 2);
        let err = apply(
            &mut state,
            &[event(
                "grid_scroll",
                vec![vec![
                    Value::from(1u64),
                    Value::from(0u64),
                    Value::from(2u64),
                    Value::from(0u64),
                    Value::from(2u64),
                    Value::from(0i64),
                    Value::from(1i64),
                ]],
            )],
        )
        .expect_err("cols is always zero in the protocol");
        assert!(
            err.to_string().contains("horizontal"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn hl_attr_define_resolves_reverse_against_the_defaults() {
        let mut state = UiState::default();
        let attrs = vec![
            (text("foreground"), Value::from(0x00_11_22u64)),
            (text("reverse"), Value::from(true)),
            (text("undercurl"), Value::from(true)),
        ];
        apply(
            &mut state,
            &[
                event(
                    "default_colors_set",
                    vec![vec![
                        Value::from(0xff_ff_ffu64),
                        Value::from(0x00_00_00u64),
                        Value::from(0xff_00_00u64),
                        Value::from(0u64),
                        Value::from(0u64),
                    ]],
                ),
                event(
                    "hl_attr_define",
                    vec![vec![
                        Value::from(3u64),
                        Value::Map(attrs),
                        Value::Map(vec![]),
                        Value::Array(vec![]),
                    ]],
                ),
            ],
        )
        .expect("apply");

        // reverse なので fg には既定の bg が、bg には定義した fg が出る。
        let style = state.hl.style(3);
        assert_eq!(style.fg, Rgb(0x00_00_00));
        assert_eq!(style.bg, Rgb(0x00_11_22));
        // 未指定の special は既定色のまま。
        assert_eq!(style.sp, Rgb(0xff_00_00));
        assert_eq!(style.underline, Underline::Undercurl);

        // 未定義の id は既定色そのもの（反転しない）。
        let plain = state.hl.style(99);
        assert_eq!(plain.fg, Rgb(0xff_ff_ff));
        assert_eq!(plain.bg, Rgb(0x00_00_00));
    }

    #[test]
    fn mode_change_drives_accepts_text_input() {
        let mut state = UiState::default();
        let outcome = apply(
            &mut state,
            &[event(
                "mode_change",
                vec![vec![Value::from("insert"), Value::from(0u64)]],
            )],
        )
        .expect("apply");
        assert!(outcome.mode_changed);
        assert!(state.mode.accepts_text_input());

        apply(
            &mut state,
            &[event(
                "mode_change",
                vec![vec![Value::from("normal"), Value::from(1u64)]],
            )],
        )
        .expect("apply");
        assert!(!state.mode.accepts_text_input());

        // cmdline_normal のような合成名も先頭で判定する。
        apply(
            &mut state,
            &[event(
                "mode_change",
                vec![vec![Value::from("cmdline_normal"), Value::from(2u64)]],
            )],
        )
        .expect("apply");
        assert!(state.mode.accepts_text_input());
    }

    #[test]
    fn mode_info_set_reads_the_shape_and_rejects_unknown_ones() {
        let mut state = UiState::default();
        let info = vec![
            (text("name"), text("insert")),
            (text("cursor_shape"), text("vertical")),
            (text("cell_percentage"), Value::from(25u64)),
            (text("attr_id"), Value::from(4u64)),
            (text("blinkon"), Value::from(500u64)),
        ];
        apply(
            &mut state,
            &[event(
                "mode_info_set",
                vec![vec![
                    Value::from(true),
                    Value::Array(vec![Value::Map(info)]),
                ]],
            )],
        )
        .expect("apply");
        assert!(state.mode.cursor_style_enabled());
        let current = state.mode.info().expect("mode 0");
        assert_eq!(current.name, "insert");
        assert_eq!(current.shape, CursorShape::Vertical);
        assert_eq!(current.cell_percentage, 25);
        assert_eq!(current.attr_id, 4);

        let bogus = vec![(text("cursor_shape"), text("diamond"))];
        apply(
            &mut state,
            &[event(
                "mode_info_set",
                vec![vec![
                    Value::from(true),
                    Value::Array(vec![Value::Map(bogus)]),
                ]],
            )],
        )
        .expect_err("an unknown cursor_shape is protocol drift");
    }

    #[test]
    fn option_set_guifont_lands_on_the_state_and_raises_the_flag() {
        let mut state = UiState::default();
        let outcome = apply(
            &mut state,
            &[event(
                "option_set",
                vec![
                    vec![Value::from("guifont"), Value::from("UDEV Gothic:h12")],
                    vec![Value::from("linespace"), Value::from(0u64)],
                ],
            )],
        )
        .expect("apply");
        assert!(outcome.font_changed);
        assert_eq!(state.guifont.as_deref(), Some("UDEV Gothic:h12"));

        // 空文字列は「未設定」。`Some("")` を渡すとフォント生成が必ず失敗する。
        apply(
            &mut state,
            &[event(
                "option_set",
                vec![vec![Value::from("guifont"), Value::from("")]],
            )],
        )
        .expect("apply");
        assert_eq!(state.guifont, None);
    }

    #[test]
    fn flush_resize_title_and_busy_are_reported() {
        let mut state = UiState::default();
        let outcome = apply(
            &mut state,
            &[
                event(
                    "grid_resize",
                    vec![vec![
                        Value::from(1u64),
                        Value::from(8u64),
                        Value::from(2u64),
                    ]],
                ),
                event("busy_start", vec![vec![]]),
                event("set_title", vec![vec![Value::from("scratch")]]),
                event("flush", vec![vec![]]),
            ],
        )
        .expect("apply");
        assert_eq!(
            outcome,
            RedrawOutcome {
                flushed: true,
                resized: true,
                title_changed: true,
                ..RedrawOutcome::default()
            }
        );
        assert_eq!((state.grid.cols(), state.grid.rows()), (8, 2));
        assert_eq!(state.title, "scratch");
        assert!(state.busy);

        apply(&mut state, &[event("busy_stop", vec![vec![]])]).expect("apply");
        assert!(!state.busy);
    }

    /// 追跡しないグリッドは捨てる。捨てそこねると寸法が上書きされて画面が壊れる。
    #[test]
    fn events_for_other_grids_are_dropped() {
        let mut state = sized(4, 1);
        let outcome = apply(
            &mut state,
            &[
                event(
                    "grid_resize",
                    vec![vec![
                        Value::from(2u64),
                        Value::from(99u64),
                        Value::from(9u64),
                    ]],
                ),
                event(
                    "grid_line",
                    vec![vec![
                        Value::from(2u64),
                        Value::from(0u64),
                        Value::from(0u64),
                        Value::Array(vec![cell("z", Some(1), None)]),
                        Value::from(false),
                    ]],
                ),
            ],
        )
        .expect("apply");
        assert!(!outcome.resized);
        assert_eq!((state.grid.cols(), state.grid.rows()), (4, 1));
        assert_eq!(state.grid.row_text(0), "");
    }

    #[test]
    fn unknown_events_are_ignored_but_broken_known_ones_are_not() {
        let mut state = sized(2, 1);
        apply(
            &mut state,
            &[event(
                "win_viewport",
                vec![vec![Value::from(1u64), Value::from(1000u64)]],
            )],
        )
        .expect("an unknown event must not break the stream");

        apply(
            &mut state,
            &[event(
                "grid_cursor_goto",
                vec![vec![Value::from(1u64), Value::from("nope")]],
            )],
        )
        .expect_err("a known event with a broken argument must fail loudly");
    }
}

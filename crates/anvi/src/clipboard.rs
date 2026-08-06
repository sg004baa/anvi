//! クリップボード操作 (DESIGN §8.2 / §9.3 / §10.4)。
//!
//! `arboard` ではなく Win32 直叩きにしている。退避 / 復元とコピー完了待ちを
//! 自分で握る必要があるため。
//!
//! nvim の `+` / `*` レジスタの実体 ([`WinClipboard`]) もここが担う。

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// `OpenClipboard` は他プロセスが握っている間失敗する。短時間リトライする。
const OPEN_TIMEOUT: Duration = Duration::from_millis(500);
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const SEQUENCE_POLL_INTERVAL: Duration = Duration::from_millis(5);

const UNICODE_TEXT: u32 = CF_UNICODETEXT.0 as u32;

/// 退避したクリップボード内容。
///
/// **`CF_UNICODETEXT` のみ**を保存する。画像 + HTML + text を同時に持つデータや
/// 遅延レンダリング形式は原理的に完全復元できないため、ここは best effort と
/// 割り切る (§8.2 / §10.4)。この割り切り自体が受け入れ済みの制約である。
#[derive(Debug)]
pub struct Snapshot(Option<String>);

/// クリップボードを開いている間だけ生きるガード。`CloseClipboard` を必ず通す。
struct Clipboard;

impl Clipboard {
    fn open() -> Result<Self> {
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            // SAFETY: 所有ウィンドウを渡さない (None) 呼び出しで、ポインタを扱わない。
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).with_context(|| {
                            format!(
                                "OpenClipboard が {} ms 以内に成功しなかった (他プロセスが占有)",
                                OPEN_TIMEOUT.as_millis()
                            )
                        });
                    }
                    std::thread::sleep(OPEN_RETRY_INTERVAL);
                }
            }
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: open() の成功と 1 対 1 で対応する 1 回だけの解放。
        // 閉じられない場合に打てる手はないため結果は捨てる。
        let _ = unsafe { CloseClipboard() };
    }
}

/// 現在のクリップボードのシーケンス番号。コピー完了待ちの基準値に使う。
pub fn sequence_number() -> u32 {
    // SAFETY: 引数なしの問い合わせ。クリップボードを開く必要もない。
    unsafe { GetClipboardSequenceNumber() }
}

/// `sequence_number()` が `baseline` から変化するまで待つ。
///
/// 変化を観測できたら `true`。固定 sleep ではなくシーケンス番号を機構にする (§8.2)。
/// 空の入力欄では `Ctrl+C` してもクリップボードは更新されず番号も変わらないため、
/// `false` は「コピー失敗」と「空欄」の両方を意味しうる。呼び出し側が §8.3 に従って
/// 解釈すること。
pub fn wait_for_change(baseline: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if sequence_number() != baseline {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(SEQUENCE_POLL_INTERVAL);
    }
}

/// `CF_UNICODETEXT` を読む。テキスト形式が無ければ `Ok(None)`。
pub fn get_text() -> Result<Option<String>> {
    let _clipboard = Clipboard::open()?;

    // SAFETY: クリップボードは開いている。形式 ID を渡すだけの問い合わせ。
    if unsafe { IsClipboardFormatAvailable(UNICODE_TEXT) }.is_err() {
        return Ok(None);
    }

    // SAFETY: 同上。返るハンドルはクリップボードの所有物であり、解放してはならない。
    let handle = unsafe { GetClipboardData(UNICODE_TEXT) }
        .context("GetClipboardData(CF_UNICODETEXT) に失敗")?;
    let hmem = HGLOBAL(handle.0);

    // SAFETY: hmem は CF_UNICODETEXT のグローバルメモリハンドル。
    let ptr = unsafe { GlobalLock(hmem) }.cast::<u16>();
    if ptr.is_null() {
        bail!("クリップボードデータの GlobalLock に失敗");
    }
    // SAFETY: hmem は有効なハンドルで、ロック中は確保サイズが変わらない。
    let capacity = unsafe { GlobalSize(hmem) } / size_of::<u16>();

    let mut len = 0usize;
    // NUL 終端まで読むが、確保サイズを超えては読まない（壊れたデータへの保険）。
    // SAFETY: len < capacity の範囲でのみ読む。
    while len < capacity && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: ptr..ptr+len は上のループで境界確認済みの初期化済み u16 列。
    let units = unsafe { std::slice::from_raw_parts(ptr, len) };
    // 他プロセスが置いた外部データなので、不正な UTF-16 は置換文字に落として通す。
    let text = String::from_utf16_lossy(units);

    // SAFETY: 直前の GlobalLock と 1 対 1 で対応する。ロック数が 0 になると
    // GlobalUnlock は「エラーなし」で FALSE を返す仕様のため、結果は捨てる。
    let _ = unsafe { GlobalUnlock(hmem) };

    Ok(Some(text))
}

/// `CF_UNICODETEXT` として書き込む。既存の内容は失われる。
pub fn set_text(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16.len() * size_of::<u16>();

    let _clipboard = Clipboard::open()?;
    // SAFETY: クリップボードは開いている。
    unsafe { EmptyClipboard() }.context("EmptyClipboard に失敗")?;

    // SAFETY: クリップボードへ渡すメモリは GMEM_MOVEABLE で確保する必要がある。
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
        .with_context(|| format!("クリップボード用に {bytes} バイトを確保できなかった"))?;

    // SAFETY: 直前に確保したハンドル。
    let dst = unsafe { GlobalLock(hmem) }.cast::<u16>();
    if dst.is_null() {
        // SAFETY: まだクリップボードへ渡していないので所有権は我々にある。
        let _ = unsafe { GlobalFree(Some(hmem)) };
        bail!("クリップボード用メモリの GlobalLock に失敗");
    }
    // SAFETY: dst は utf16.len() 要素分の領域を指し、確保直後なので領域は重複しない。
    unsafe { std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len()) };
    // SAFETY: 直前の GlobalLock と対応。get_text と同じ理由で結果は捨てる。
    let _ = unsafe { GlobalUnlock(hmem) };

    // SAFETY: 成功するとメモリの所有権はシステムへ移る。失敗時のみ我々が解放する。
    if let Err(e) = unsafe { SetClipboardData(UNICODE_TEXT, Some(HANDLE(hmem.0))) } {
        // SAFETY: 失敗したので所有権は移っていない。
        let _ = unsafe { GlobalFree(Some(hmem)) };
        return Err(e).context("SetClipboardData(CF_UNICODETEXT) に失敗");
    }
    Ok(())
}

/// 現在のクリップボード内容を退避する (§8.2)。
pub fn snapshot() -> Result<Snapshot> {
    Ok(Snapshot(get_text()?))
}

/// 退避した内容へ戻す。best effort (→ [`Snapshot`])。
pub fn restore(snapshot: &Snapshot) -> Result<()> {
    match &snapshot.0 {
        Some(text) => set_text(text),
        None => {
            // 元がテキストでなかった場合、その中身は我々の set_text による
            // EmptyClipboard で既に失われている。我々が置いたテキストを残すほうが
            // 害が大きいので空にする。
            let _clipboard = Clipboard::open()?;
            // SAFETY: クリップボードは開いている。
            unsafe { EmptyClipboard() }.context("退避内容の復元 (空化) に失敗")
        }
    }
}

/// nvim の `+` / `*` レジスタを Win32 クリップボードへ繋ぐ実体 (DESIGN §5.6)。
#[derive(Debug)]
pub struct WinClipboard;

impl anvi_core::Clipboard for WinClipboard {
    fn get(&self) -> Result<String> {
        // CF_UNICODETEXT が無い = 貼るテキストが無い。空文字列がその正直な答えで、
        // 失敗の握り潰しではない。
        Ok(get_text()?.unwrap_or_default())
    }

    fn set(&self, text: &str) -> Result<()> {
        set_text(text)
    }
}

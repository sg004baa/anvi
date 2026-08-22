//! UI Automation 経由のテキスト取得と書き戻し (DESIGN §8 / §9)。
//!
//! COM のアパートメント地雷を避けるため、UIA は専用の MTA スレッドで回す (§11.2)。
//! `IUIAutomation` / `IUIAutomationElement` などの COM ポインタはこのスレッドから
//! 一切出さない。外との受け渡しは `std::sync::mpsc` で行い、チャンネルに載るのは
//! プレーンなデータ (行配列 / HWND) だけである。したがって `unsafe impl Send` は不要。

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anvi_core::text::{to_crlf, to_lines};
use anyhow::{Context as _, Result, anyhow, bail};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};

use crate::clipboard;
use crate::keys;

/// `Ctrl+C` の完了待ちの上限。タイムアウトの扱いは §8.3 に従う。
const COPY_TIMEOUT: Duration = Duration::from_millis(400);

/// 貼り付け後、クリップボードを復元するまでの待ち。
///
/// コピーと違い「貼り付けが終わった」ことを観測できる Win32 の信号は存在しない
/// （読み取りではシーケンス番号が変わらない）ため、ここだけは固定待ちになる。
const PASTE_SETTLE: Duration = Duration::from_millis(150);

/// 取得できたテキストと、フォーカスを戻すべきウィンドウ。
#[derive(Debug, Clone)]
pub struct Captured {
    pub lines: Vec<String>,
    pub hwnd: isize,
}

/// UIA スレッド内にだけ存在する書き戻し対象。COM ポインタを含むため外に出さない。
struct Target {
    element: IUIAutomationElement,
    /// 取得時の RuntimeId。取得できない要素は COM identity で照合する。
    runtime_id: Option<Vec<i32>>,
    hwnd: isize,
}

enum Job {
    Capture,
    WriteBack(Vec<String>),
}

enum Reply {
    Capture(Result<Option<Captured>>),
    WriteBack(Result<()>),
}

/// UIA スレッドへのハンドル。メソッドはすべて応答待ちの同期呼び出し。
pub struct Uia {
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
}

impl Uia {
    /// MTA スレッドを起動し、`CUIAutomation` の生成まで済ませる。
    pub fn start() -> Result<Self> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("anvi-uia".to_owned())
            .spawn(move || thread_main(&job_rx, &reply_tx, &ready_tx))
            .context("UIA スレッドを起動できなかった")?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: job_tx,
                replies: reply_rx,
            }),
            Ok(Err(e)) => Err(e).context("UIA スレッドの初期化に失敗"),
            Err(_) => bail!("UIA スレッドが初期化結果を返さずに終了した"),
        }
    }

    /// フォーカス中の入力欄からテキストを取得する (§8)。
    ///
    /// `Ok(None)` は「編集対象が無い」。呼び出し側は何もせず Idle に戻ること (§8.3)。
    pub fn capture(&self) -> Result<Option<Captured>> {
        match self.request(Job::Capture)? {
            Reply::Capture(result) => result,
            Reply::WriteBack(_) => bail!("UIA スレッドの応答が要求と一致しない (capture)"),
        }
    }

    /// 取得時の対象へ書き戻す (§9)。呼び出し前にフォーカスを戻しておくこと。
    pub fn write_back(&self, lines: &[String]) -> Result<()> {
        match self.request(Job::WriteBack(lines.to_vec()))? {
            Reply::WriteBack(result) => result,
            Reply::Capture(_) => bail!("UIA スレッドの応答が要求と一致しない (write_back)"),
        }
    }

    fn request(&self, job: Job) -> Result<Reply> {
        self.jobs
            .send(job)
            .map_err(|_| anyhow!("UIA スレッドが既に終了している"))?;
        self.replies
            .recv()
            .map_err(|_| anyhow!("UIA スレッドが応答を返さずに終了した"))
    }
}

/// このスレッドの COM を MTA で初期化し、Drop で必ず解除する。
struct ComMta;

impl ComMta {
    fn init() -> Result<Self> {
        // SAFETY: このスレッドで最初の COM 初期化。既に別モードで初期化されていれば
        // RPC_E_CHANGED_MODE が返り、下の ok() で Err になる（黙って続行しない）。
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("CoInitializeEx(COINIT_MULTITHREADED) に失敗")?;
        Ok(Self)
    }
}

impl Drop for ComMta {
    fn drop(&mut self) {
        // SAFETY: init() の成功と 1 対 1 で対応する解除。
        unsafe { CoUninitialize() };
    }
}

fn thread_main(jobs: &Receiver<Job>, replies: &Sender<Reply>, ready: &Sender<Result<()>>) {
    // _com は uia より前に宣言する。ローカルは宣言の逆順に drop されるため、
    // COM ポインタの解放が CoUninitialize より先になる。
    let _com = match ComMta::init() {
        Ok(com) => com,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    // SAFETY: MTA 初期化済みのこのスレッド上で生成する。以後このポインタは
    // スレッド外へ出さない。
    let uia: IUIAutomation =
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
            Ok(uia) => uia,
            Err(e) => {
                let _ = ready.send(Err(e).context("CUIAutomation の生成に失敗"));
                return;
            }
        };

    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut target: Option<Target> = None;
    while let Ok(job) = jobs.recv() {
        let reply = match job {
            Job::Capture => Reply::Capture(capture(&uia, &mut target)),
            Job::WriteBack(lines) => {
                let result = write_back(&uia, target.as_ref(), &lines);
                // セッションは終わった。stale な要素を抱え続けない。
                target = None;
                Reply::WriteBack(result)
            }
        };
        if replies.send(reply).is_err() {
            break;
        }
    }
}

/// 編集可能な対象を確認し、クリップボードから取得してスレッド内に保持する。
fn capture(uia: &IUIAutomation, slot: &mut Option<Target>) -> Result<Option<Captured>> {
    // 前セッションの対象は必ず捨てる。取得に失敗したあとで古い対象に書き戻すのは事故。
    *slot = None;

    // SAFETY: uia はこのスレッドで生成した有効な COM ポインタ。
    let Ok(element) = (unsafe { uia.GetFocusedElement() }) else {
        // フォーカス要素が特定できない → 何もせず Idle に戻る (§8.3)。
        tracing::debug!("GetFocusedElement に失敗: 編集対象なしとして扱う");
        return Ok(None);
    };

    let Some(hwnd) = target_hwnd(&element) else {
        tracing::debug!("対象の HWND を特定できない: 編集対象なしとして扱う");
        return Ok(None);
    };

    // 編集系と確認できない相手には Ctrl+A / Ctrl+C を注入しない (§8.3)。
    if !is_edit_like(&element) {
        tracing::debug!("編集系の要素と確認できない: キー注入せず編集対象なしとして扱う");
        return Ok(None);
    }

    let text = capture_via_clipboard()
        .context("編集対象からクリップボード経由でテキストを取得できなかった")?;
    Ok(Some(retain(slot, element, hwnd, &text)))
}

fn retain(
    slot: &mut Option<Target>,
    element: IUIAutomationElement,
    hwnd: isize,
    text: &str,
) -> Captured {
    let lines = to_lines(text);
    let runtime_id = runtime_id(&element);
    tracing::info!(
        route = "Clipboard",
        lines = lines.len(),
        chars = text.chars().count(),
        hwnd = format_args!("{hwnd:#x}"),
        "captured"
    );
    *slot = Some(Target {
        element,
        runtime_id,
        hwnd,
    });
    Captured { lines, hwnd }
}

/// 取得時と同じ編集対象へクリップボード貼り付けで書き戻す (§9.1)。
fn write_back(uia: &IUIAutomation, target: Option<&Target>, lines: &[String]) -> Result<()> {
    let target = target.context("書き戻し対象が保持されていない")?;
    ensure_same_edit_target(uia, target)?;
    // 結合は常に CRLF（`CF_UNICODETEXT` の慣習。旧来の EDIT コントロールもこれを要求する）。
    paste_back(target, &to_crlf(lines))
}

/// フォーカス中の要素が取得時と同じ編集対象であることを確認する。
fn ensure_same_edit_target(uia: &IUIAutomation, target: &Target) -> Result<()> {
    // SAFETY: uia はこのスレッドで生成した有効な COM ポインタ。
    let focused = unsafe { uia.GetFocusedElement() }
        .context("書き戻し先のフォーカス要素を取得できなかった")?;
    if !is_edit_like(&focused) {
        bail!("書き戻し先が編集可能な要素ではないため、キー注入を中止した");
    }

    let same = if let Some(want) = target.runtime_id.as_deref() {
        // RuntimeId は COM wrapper ではなく論理要素の identity。provider が同じ論理要素の
        // wrapper を作り直しても同じ ID を返すため、正規な再取得はここで一致する。
        runtime_id(&focused).is_some_and(|actual| actual == want)
    } else {
        // RuntimeId を公開しない provider では UIA 自身の element identity 比較を使う。
        // SAFETY: focused と target.element は同じ MTA 上の有効な UIA 要素。
        unsafe { uia.CompareElements(&focused, &target.element) }
            .context("書き戻し先の UIA 要素を照合できなかった")?
            .as_bool()
    };
    if !same {
        bail!("書き戻し先が取得時の編集対象と一致しないため、キー注入を中止した");
    }
    Ok(())
}

/// ControlType が Edit / Document で、かつキーボードフォーカス可能か (§8.3)。
fn is_edit_like(element: &IUIAutomationElement) -> bool {
    // SAFETY: element は生きた UIA 要素。
    let Ok(control_type) = (unsafe { element.CurrentControlType() }) else {
        return false;
    };
    if control_type != UIA_EditControlTypeId && control_type != UIA_DocumentControlTypeId {
        return false;
    }
    // SAFETY: 同上。
    unsafe { element.CurrentIsKeyboardFocusable() }.is_ok_and(|focusable| focusable.as_bool())
}

/// `Ctrl+A` → `Ctrl+C` → クリップボード読み取り (§8.2 / §8.3)。
///
/// 呼び出し前に対象が編集系であることを確認済みであること。
fn capture_via_clipboard() -> Result<String> {
    let saved = clipboard::snapshot().context("クリップボードの退避に失敗")?;
    let baseline = clipboard::sequence_number();
    let captured = copy_selection(baseline);
    // Ctrl+C が空振りした（空欄）場合はクリップボードが手つかずなので、復元と称して
    // 触らない。触ると、テキスト以外の形式で無事に残っていた内容を壊してしまう。
    if clipboard::sequence_number() != baseline {
        // 復元は best effort。失敗しても取得結果は活かす (§8.2)。
        if let Err(e) = clipboard::restore(&saved) {
            tracing::warn!("クリップボードの復元に失敗 (best effort): {e:#}");
        }
    }
    captured
}

fn copy_selection(baseline: u32) -> Result<String> {
    keys::select_all()?;
    keys::copy()?;
    if !clipboard::wait_for_change(baseline, COPY_TIMEOUT) {
        // 空の入力欄では Ctrl+C してもクリップボードは更新されない。ここへ来るのは
        // 対象が編集系と確認できている場合だけなので、空文字として続行する (§8.3)。
        tracing::debug!("Ctrl+C 後にクリップボードが更新されなかった: 空欄として扱う");
        return Ok(String::new());
    }
    clipboard::get_text()?.context("Ctrl+C 後のクリップボードに CF_UNICODETEXT が無かった")
}

/// クリップボード貼り付けによる書き戻し (§9.3)。
///
/// フォーカスは呼び出し側 (controller) が既に対象へ戻している。
fn paste_back(target: &Target, text: &str) -> Result<()> {
    ensure_target_foreground(target)?;
    let saved = clipboard::snapshot().context("クリップボードの退避に失敗")?;
    let pasted = paste_text(text);
    if let Err(e) = clipboard::restore(&saved) {
        tracing::warn!("クリップボードの復元に失敗 (best effort): {e:#}");
    }
    pasted
}

/// キー注入の直前に、対象のトップレベルウィンドウが本当に前面かを確かめる。
///
/// フォーカス復帰 (§9.3 手順 3) が失敗したまま `Ctrl+A` / `Ctrl+V` を撃つと、
/// 無関係なウィンドウの内容を破壊する。取得時に HWND を保持しているのはこのため。
fn ensure_target_foreground(target: &Target) -> Result<()> {
    let expected = root_window(target.hwnd);
    let actual = root_window(crate::focus::foreground_window());
    if expected == 0 || expected != actual {
        bail!(
            "書き戻し対象 (hwnd={:#x}) が前面ではない (前面の root=hwnd={:#x}) ため、キー注入を中止した",
            target.hwnd,
            actual
        );
    }
    Ok(())
}

/// トップレベルウィンドウ。対象が子コントロールの HWND を持つ場合があるため、
/// 前面判定は root 同士で行う。
fn root_window(hwnd: isize) -> isize {
    // SAFETY: 無効なハンドルを渡しても NULL を返すだけの問い合わせ。
    unsafe { GetAncestor(HWND(hwnd as *mut c_void), GA_ROOT) }.0 as isize
}

fn paste_text(text: &str) -> Result<()> {
    clipboard::set_text(text).context("編集結果をクリップボードへ置けなかった")?;
    // フォーカスが戻った時点で選択範囲は失われているため、必ず選択し直す (§9.3)。
    keys::select_all()?;
    keys::paste()?;
    std::thread::sleep(PASTE_SETTLE);
    Ok(())
}

/// フォーカスを戻すべきウィンドウ。要素が HWND を持たない場合は前面ウィンドウ。
fn target_hwnd(element: &IUIAutomationElement) -> Option<isize> {
    // SAFETY: element は生きた UIA 要素。
    if let Ok(hwnd) = unsafe { element.CurrentNativeWindowHandle() }
        && !hwnd.0.is_null()
    {
        return Some(hwnd.0 as isize);
    }
    // ブラウザ内の <input> のように、要素が自分の HWND を持たないことがある。
    let foreground = crate::focus::foreground_window();
    (foreground != 0).then_some(foreground)
}

fn runtime_id(element: &IUIAutomationElement) -> Option<Vec<i32>> {
    // SAFETY: element は生きた UIA 要素。返る SAFEARRAY の所有権は呼び出し側にある。
    let array = unsafe { element.GetRuntimeId() }.ok()?;
    if array.is_null() {
        return None;
    }
    // SAFETY: array は GetRuntimeId が返した有効な SAFEARRAY。
    let ids = unsafe { read_i32_array(array) };
    // SAFETY: 所有権は我々にあるため、ここで必ず解放する。
    let _ = unsafe { SafeArrayDestroy(array) };
    // 空の RuntimeId は同一性判定に使えない（別の空同士が一致してしまう）。
    ids.filter(|ids| !ids.is_empty())
}

/// # Safety
///
/// `array` は有効な `SAFEARRAY` を指していること。
unsafe fn read_i32_array(array: *mut SAFEARRAY) -> Option<Vec<i32>> {
    // SAFETY: 呼び出し側の契約により array は有効な SAFEARRAY。次元数と要素サイズを
    // 確認してから i32 として読むため、SafeArrayGetElement の書き込み先も足りている。
    unsafe {
        if SafeArrayGetDim(array) != 1 {
            return None;
        }
        // RuntimeId は VT_I4 の配列。要素サイズを確認してから i32 として読む。
        if (*array).cbElements as usize != size_of::<i32>() {
            return None;
        }
        let lower = SafeArrayGetLBound(array, 1).ok()?;
        let upper = SafeArrayGetUBound(array, 1).ok()?;
        let mut ids = Vec::new();
        for index in lower..=upper {
            let mut id = 0i32;
            SafeArrayGetElement(array, &index, (&raw mut id).cast()).ok()?;
            ids.push(id);
        }
        Some(ids)
    }
}

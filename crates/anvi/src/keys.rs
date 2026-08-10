//! キー注入 (DESIGN §8.2 / §9.3)。
//!
//! ホットキー押下直後は Ctrl / Alt が、`ZZ` 直後は Shift が物理的に押されたまま
//! である。そこへ `Ctrl+A` を注入すると対象アプリには `Ctrl+Shift+A` 等が届いて
//! しまうため、注入前に必ず物理修飾キーの解放を待つ。

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_A, VK_C, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT, VK_V,
};

/// 解放待ちの上限。超えたら注入せずエラーにする。誤った修飾キー付きのキーを
/// 対象アプリへ撃ち込むほうが、取得/書き戻しの失敗より害が大きい。
const RELEASE_TIMEOUT: Duration = Duration::from_millis(1500);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 監視する物理修飾キー。Win キーは左右を個別に見る必要がある。
const MODIFIERS: [VIRTUAL_KEY; 5] = [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN];

/// 物理修飾キー (Ctrl / Shift / Alt / Win) がすべて離れるまで待つ。
pub fn wait_modifiers_released() -> Result<()> {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        match held_modifier() {
            None => return Ok(()),
            Some(vk) => {
                if Instant::now() >= deadline {
                    bail!(
                        "修飾キー (vk={}) が {} ms 以内に解放されなかったため、キー注入を中止した",
                        vk.0,
                        RELEASE_TIMEOUT.as_millis()
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// `Ctrl+A`。フォーカスが戻った時点で選択範囲は失われているため、貼り付け前にも
/// 必ず呼ぶ (§9.3)。
pub fn select_all() -> Result<()> {
    ctrl_chord(VK_A).context("Ctrl+A の注入に失敗")
}

/// `Ctrl+C`。
pub fn copy() -> Result<()> {
    ctrl_chord(VK_C).context("Ctrl+C の注入に失敗")
}

/// `Ctrl+V`。
pub fn paste() -> Result<()> {
    ctrl_chord(VK_V).context("Ctrl+V の注入に失敗")
}

fn ctrl_chord(vk: VIRTUAL_KEY) -> Result<()> {
    wait_modifiers_released()?;
    send(&[
        key_event(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        key_event(vk, KEYBD_EVENT_FLAGS(0)),
        key_event(vk, KEYEVENTF_KEYUP),
        key_event(VK_CONTROL, KEYEVENTF_KEYUP),
    ])
}

/// いま押されている修飾キーを 1 つ返す。押されていなければ `None`。
fn held_modifier() -> Option<VIRTUAL_KEY> {
    MODIFIERS.into_iter().find(|vk| is_down(*vk))
}

fn is_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: GetAsyncKeyState は任意の仮想キーコードを受け付ける純粋な問い合わせで、
    // ポインタを一切扱わない。
    let state = unsafe { GetAsyncKeyState(i32::from(vk.0)) };
    // 最上位ビットのみが「現在押下中」。最下位ビットは「前回の呼び出し以降に
    // 押された」であり、ここでは見てはいけない。
    (state as u16) & 0x8000 != 0
}

fn key_event(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    // SAFETY: `inputs` は有効な INPUT の連続領域で、cbsize は実際の要素サイズと一致する。
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) } as usize;
    if sent == inputs.len() {
        return Ok(());
    }
    // UIPI（昇格プロセスが前面のとき）などで注入がブロックされた場合ここに来る。
    let err = windows::core::Error::from_thread();

    // 受理済みプレフィックスに key-down のまま up が届いていない VK が残ると、
    // Ctrl / Alt 等が OS 側で押しっぱなしにラッチする。補償の key-up を注入してから
    // 失敗を返す。補償の結果もエラー文言に含める。
    let mut held: Vec<VIRTUAL_KEY> = Vec::new();
    for input in &inputs[..sent] {
        // SAFETY: 本モジュールの INPUT はすべて key_event() が作る INPUT_KEYBOARD。
        let ki = unsafe { input.Anonymous.ki };
        if ki.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0 {
            held.retain(|vk| *vk != ki.wVk);
        } else if !held.contains(&ki.wVk) {
            held.push(ki.wVk);
        }
    }
    let mut note = String::new();
    if !held.is_empty() {
        // 押した順の逆で離す。
        let ups: Vec<INPUT> = held
            .iter()
            .rev()
            .map(|vk| key_event(*vk, KEYEVENTF_KEYUP))
            .collect();
        // SAFETY: `ups` は有効な INPUT の連続領域で、cbsize は実際の要素サイズと一致する。
        let up_sent = unsafe { SendInput(&ups, size_of::<INPUT>() as i32) } as usize;
        note = if up_sent == ups.len() {
            format!("（押下ラッチ防止の key-up {} 件を補償注入した）", ups.len())
        } else {
            format!(
                "（補償の key-up も {} 件中 {up_sent} 件しか受け付けられず、修飾キーが押下ラッチしている可能性がある）",
                ups.len()
            )
        };
    }
    Err(err).with_context(|| {
        format!(
            "SendInput が {} 件中 {sent} 件しか受け付けなかった{note}",
            inputs.len()
        )
    })
}

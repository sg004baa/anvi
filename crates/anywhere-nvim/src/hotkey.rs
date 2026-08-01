//! グローバルホットキー（DESIGN 11.2）。
//!
//! `global-hotkey` は Windows では登録したスレッドで win32 メッセージループが
//! 回っていることを要求する。したがって [`Hotkeys::register`] は winit のループが
//! 回るスレッド（= main スレッド）から呼ぶこと。
//!
//! イベントはポーリングしない。ハンドラを付けないと `global-hotkey` は無制限
//! チャンネルに積み続けるので、そこも塞ぐ意味がある。

use anyhow::Context as _;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::gui::{ProxyHandle, UserEvent};

/// v1 のホットキーは `Ctrl+Alt+E` 固定。設定 GUI も設定ファイルも v1 の
/// スコープ外（DESIGN 2）なので、変えたければここを書き換えて再ビルドする。
const MODIFIERS: Modifiers = Modifiers::CONTROL.union(Modifiers::ALT);
const CODE: Code = Code::KeyE;

pub struct Hotkeys {
    /// 生かしておく必要がある。drop すると登録が解除される。
    _manager: GlobalHotKeyManager,
}

impl Hotkeys {
    pub fn register(proxy: ProxyHandle) -> anyhow::Result<Self> {
        let manager =
            GlobalHotKeyManager::new().context("failed to create the global hotkey manager")?;
        let hotkey = HotKey::new(Some(MODIFIERS), CODE);
        manager
            .register(hotkey)
            .with_context(|| format!("failed to register the global hotkey {hotkey}"))?;
        let id = hotkey.id();

        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            // 離鍵でも同じ id が飛んでくる。押下だけを 1 回のセッション操作として扱う。
            if event.id != id || event.state != HotKeyState::Pressed {
                return;
            }
            proxy.send(UserEvent::Hotkey);
        }));

        tracing::info!(hotkey = %hotkey, "global hotkey registered");
        Ok(Self { _manager: manager })
    }
}

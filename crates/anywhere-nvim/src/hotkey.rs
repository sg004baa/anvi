//! グローバルホットキー（DESIGN 11.2）。
//!
//! `global-hotkey` は Windows では登録したスレッドで win32 メッセージループが
//! 回っていることを要求する。したがって `register()` はメッセージポンプスレッド
//! （= main スレッド）から呼ぶこと。

use std::sync::mpsc::Sender;

use anyhow::Context as _;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::controller::Cmd;

/// v1 のホットキーは `Ctrl+Alt+E` 固定。設定 GUI も設定ファイルも v1 の
/// スコープ外（DESIGN 2）なので、変えたければここを書き換えて再ビルドする。
const MODIFIERS: Modifiers = Modifiers::CONTROL.union(Modifiers::ALT);
const CODE: Code = Code::KeyE;

pub struct Hotkeys {
    /// 生かしておく必要がある。drop すると登録が解除される。
    _manager: GlobalHotKeyManager,
    id: u32,
}

impl Hotkeys {
    pub fn register() -> anyhow::Result<Self> {
        let manager =
            GlobalHotKeyManager::new().context("failed to create the global hotkey manager")?;
        let hotkey = HotKey::new(Some(MODIFIERS), CODE);
        manager
            .register(hotkey)
            .with_context(|| format!("failed to register the global hotkey {hotkey}"))?;
        tracing::info!(hotkey = %hotkey, "global hotkey registered");
        Ok(Self {
            _manager: manager,
            id: hotkey.id(),
        })
    }

    /// 溜まっているイベントを吐き出す。`DispatchMessageW` の直後に呼ぶ。
    pub fn drain(&self, tx: &Sender<Cmd>) {
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.id != self.id || event.state != HotKeyState::Pressed {
                continue;
            }
            if tx.send(Cmd::Hotkey).is_err() {
                tracing::error!("controller is gone; hotkey dropped");
                return;
            }
        }
    }
}

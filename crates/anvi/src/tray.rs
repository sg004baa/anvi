//! トレイアイコン（DESIGN 12 章ステップ 2）。
//!
//! メニューは自動起動のトグルと `Exit` の 2 項目。設定 GUI は v1 のスコープ外（DESIGN 2）。
//! `tray-icon` は内部で隠しウィンドウを作るため、winit のループが回るスレッド
//! （= main スレッド）で生成すること。
//!
//! イベントはポーリングしない。`muda` / `tray-icon` は「ハンドラ未設定なら無制限
//! チャンネルに積む」実装なので、汲まない経路を残すとアイコン上のマウス移動だけで
//! メモリが増え続ける。両方にハンドラを付けて塞ぐ。

use anyhow::Context as _;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::autostart;
use crate::gui::{ProxyHandle, UserEvent};

const EXIT_ID: &str = "anvi.exit";
const STARTUP_ID: &str = "anvi.startup";

/// exe に埋め込んだアイコンの名前 ID。`build.rs` の `set_icon` が付ける既定値で、
/// エクスプローラやインストーラが出すアイコンと同一の実体を指す。
const ICON_ID: u16 = 1;
/// 読み出すサイズ（px）。ICO には 16〜256 が入っているので、通知領域の実寸を選ぶ。
const ICON_SIZE: u32 = 32;

pub struct Tray {
    /// 生かしておく必要がある。drop するとアイコンが消える。
    _icon: TrayIcon,
    /// 自動起動のチェック項目。muda がクリックで勝手にトグルするので、
    /// レジストリ操作に失敗したときはここを戻して画面と実態を合わせる。
    startup: CheckMenuItem,
}

impl Tray {
    pub fn new(proxy: ProxyHandle) -> anyhow::Result<Self> {
        // 実際の登録状態を初期値にする。インストーラの startup タスクで入れた場合も
        // 同じレジストリ値なので、そのままチェックが付く。
        let enabled = autostart::is_enabled()
            .inspect_err(|err| tracing::error!(%err, "failed to read the autostart entry"))
            .unwrap_or(false);
        let startup = CheckMenuItem::with_id(STARTUP_ID, "サインイン時に起動", true, enabled, None);
        let exit = MenuItem::with_id(EXIT_ID, "Exit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&startup, &PredefinedMenuItem::separator(), &exit])
            .context("failed to build the tray menu")?;
        let startup_id = startup.id().clone();
        let exit_id = exit.into_id();

        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == exit_id {
                proxy.send(UserEvent::Quit);
            } else if event.id == startup_id {
                // ハンドラは `Send` を要求されるためメニュー項目を掴めない。
                // 実処理はイベントループのスレッドへ回す。
                proxy.send(UserEvent::ToggleAutostart);
            } else {
                tracing::debug!(id = %event.id.0, "unknown menu item");
            }
        }));
        // クリックにもホバーにも用は無い。溜めないためだけに受け取る。
        TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
            tracing::trace!(?event, "tray icon event ignored");
        }));

        let icon = TrayIconBuilder::new()
            .with_tooltip("anvi")
            .with_icon(icon()?)
            .with_menu(Box::new(menu))
            .build()
            .context("failed to create the tray icon")?;

        Ok(Self {
            _icon: icon,
            startup,
        })
    }

    /// チェックの新しい状態をレジストリへ反映する。失敗したらチェックを戻す。
    pub fn apply_autostart(&self) {
        let want = self.startup.is_checked();
        match autostart::set(want) {
            Ok(()) => tracing::info!(enabled = want, "autostart updated"),
            Err(err) => {
                tracing::error!(%err, "failed to update the autostart entry");
                self.startup.set_checked(!want);
            }
        }
    }
}

/// exe に埋め込んだアイコンを読む。絵の出典は `scripts/make-icon.py`。
fn icon() -> anyhow::Result<Icon> {
    Icon::from_resource(ICON_ID, Some((ICON_SIZE, ICON_SIZE)))
        .context("failed to load the embedded tray icon")
}

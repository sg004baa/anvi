//! トレイアイコン（DESIGN 12 章ステップ 2）。
//!
//! メニューは `Exit` の 1 項目だけ。設定 GUI は v1 のスコープ外（DESIGN 2）。
//! `tray-icon` は内部で隠しウィンドウを作るため、メッセージポンプスレッドで
//! 生成すること。

use anyhow::Context as _;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const EXIT_ID: &str = "anywhere.exit";

/// アイコンの辺（px）。
const SIZE: i32 = 32;
/// 角丸の半径（px）。
const RADIUS: i32 = 6;
const BG: [u8; 4] = [0x24, 0x2b, 0x33, 0xff];
const FG: [u8; 4] = [0x8e, 0xc0, 0x7c, 0xff];
/// 5x7 ビットマップの "A"。外部アセットもネットワークも要らないので絵はコードで持つ。
const GLYPH: [u8; 7] = [
    0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
];
const GLYPH_W: i32 = 5;
const GLYPH_H: i32 = 7;
/// 5x7 を 4 倍して 20x28。32x32 の中に収まる。
const GLYPH_SCALE: i32 = 4;

pub struct Tray {
    /// 生かしておく必要がある。drop するとアイコンが消える。
    _icon: TrayIcon,
    exit_id: MenuId,
}

impl Tray {
    pub fn new() -> anyhow::Result<Self> {
        let exit = MenuItem::with_id(EXIT_ID, "Exit", true, None);
        let menu = Menu::new();
        menu.append(&exit)
            .context("failed to build the tray menu")?;

        let icon = TrayIconBuilder::new()
            .with_tooltip("anywhere-nvim")
            .with_icon(icon()?)
            .with_menu(Box::new(menu))
            .build()
            .context("failed to create the tray icon")?;

        Ok(Self {
            _icon: icon,
            exit_id: exit.into_id(),
        })
    }

    /// 溜まっているメニューイベントを吐き出す。`Exit` が押されていれば `true`。
    /// `DispatchMessageW` の直後に呼ぶ。
    pub fn exit_requested(&self) -> bool {
        let mut requested = false;
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.exit_id {
                requested = true;
            } else {
                tracing::debug!(id = %event.id.0, "unknown menu item");
            }
        }
        requested
    }
}

/// 角丸の四角に "A" を載せた 32x32 RGBA を生成する。
fn icon() -> anyhow::Result<Icon> {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let mut put = |x: i32, y: i32, color: &[u8; 4]| {
        let offset = ((y * SIZE + x) * 4) as usize;
        rgba[offset..offset + 4].copy_from_slice(color);
    };

    for y in 0..SIZE {
        for x in 0..SIZE {
            if inside_rounded(x, y) {
                put(x, y, &BG);
            }
        }
    }

    let ox = (SIZE - GLYPH_W * GLYPH_SCALE) / 2;
    let oy = (SIZE - GLYPH_H * GLYPH_SCALE) / 2;
    for (row, bits) in GLYPH.iter().enumerate() {
        for col in 0..GLYPH_W {
            if bits & (1 << (GLYPH_W - 1 - col)) == 0 {
                continue;
            }
            for dy in 0..GLYPH_SCALE {
                for dx in 0..GLYPH_SCALE {
                    put(
                        ox + col * GLYPH_SCALE + dx,
                        oy + row as i32 * GLYPH_SCALE + dy,
                        &FG,
                    );
                }
            }
        }
    }

    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).context("failed to build the tray icon bitmap")
}

/// 角の外側（透明にする画素）を弾く。
fn inside_rounded(x: i32, y: i32) -> bool {
    let center = |v: i32| {
        if v < RADIUS {
            Some(RADIUS)
        } else if v >= SIZE - RADIUS {
            Some(SIZE - 1 - RADIUS)
        } else {
            None
        }
    };
    let (Some(cx), Some(cy)) = (center(x), center(y)) else {
        return true;
    };
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= RADIUS * RADIUS
}

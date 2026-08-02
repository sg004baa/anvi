//! サインイン時の自動起動（`HKCU\...\CurrentVersion\Run`）。
//!
//! インストーラの `startup` タスクと **同じ値名を読み書きする**。どちらから設定しても
//! 同じ 1 つのエントリなので、トレイのチェックはインストーラで入れた設定もそのまま映す。
//! portable / scoop で入れた場合はインストーラが無いので、ここが唯一の入口になる。
//!
//! 値は「現在の exe の絶対パス」。別の場所へ置き直したら、そのビルドで入れ直すこと。

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 値名。インストーラ（`installer/anvi.iss`）と一致させること。
const VALUE_NAME: &str = "anvi";

/// 開いている間だけ生きるレジストリキー。`RegCloseKey` を必ず通す。
struct Key(HKEY);

impl Key {
    fn open(access: REG_SAM_FLAGS) -> Result<Self> {
        let sub = wide(RUN_KEY);
        let mut key = HKEY::default();
        // SAFETY: sub は NUL 終端の生存スライス。key は出力先のローカル変数。
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                access,
                &mut key,
            )
        };
        if status != ERROR_SUCCESS {
            bail!("RegOpenKeyExW({RUN_KEY}) failed: {status:?}");
        }
        Ok(Self(key))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: open で得た有効なハンドル。二重クローズはしない。
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// 自動起動が登録されているか。
pub fn is_enabled() -> Result<bool> {
    let key = Key::open(KEY_QUERY_VALUE)?;
    let name = wide(VALUE_NAME);
    // SAFETY: name は NUL 終端の生存スライス。中身は要らないので出力先は全て None。
    let status = unsafe { RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, None, None, None) };
    // WIN32_ERROR の定数は match のパターンに使えない（束縛になってしまう）ので比較する。
    if status == ERROR_SUCCESS {
        return Ok(true);
    }
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    bail!("RegQueryValueExW({VALUE_NAME}) failed: {status:?}");
}

/// 自動起動を登録 / 解除する。値は現在の exe の絶対パス。
pub fn set(enabled: bool) -> Result<()> {
    let key = Key::open(KEY_SET_VALUE)?;
    let name = wide(VALUE_NAME);
    if !enabled {
        // SAFETY: name は NUL 終端の生存スライス。
        let status = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
        // 元から無いのは「解除済み」であって失敗ではない。
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        bail!("RegDeleteValueW({VALUE_NAME}) failed: {status:?}");
    }

    let exe = std::env::current_exe().context("current_exe() failed")?;
    let exe = exe
        .to_str()
        .with_context(|| format!("the executable path is not UTF-8: {}", exe.display()))?;
    // 空白入りのパスでも 1 引数として扱われるよう引用する。インストーラも同じ形で書く。
    let value = wide(&format!("\"{exe}\""));
    let bytes = std::mem::size_of_val(value.as_slice());
    // SAFETY: name と value は NUL 終端の生存スライス。バイト列は value そのもので、
    // 終端の NUL を含む（REG_SZ はそれを要求する）。
    let status = unsafe {
        RegSetValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                value.as_ptr().cast::<u8>(),
                bytes,
            )),
        )
    };
    if status != ERROR_SUCCESS {
        bail!("RegSetValueExW({VALUE_NAME}) failed: {status:?}");
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

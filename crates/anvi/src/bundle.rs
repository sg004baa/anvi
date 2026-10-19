//! 同梱物のレイアウト解決（DESIGN 5.3）。
//!
//! 探索パスは持たない。`current_exe()` の隣に固定レイアウトで置かれている前提で、
//! 無ければその場で落ちる。システムにインストールされた nvim を拾うと
//! 「なぜかユーザーの設定が読まれる」という最悪の事故になるため（DESIGN 5.2）。
//!
//! ```text
//! anvi.exe
//! runtime/init.lua
//! runtime/lua/anvi/init.lua
//! runtime/lua/anvi/history.lua
//! nvim/bin/nvim.exe
//! nvim/share/nvim/runtime/   (同梱 nvim 自身の runtime。exe 相対で解決される)
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

/// `NVIM_APPNAME`。設定ディレクトリの名前空間ごと隔離する（DESIGN 5.2）。
pub const APPNAME: &str = "anvi";

/// 同梱物の実体パス。すべて存在確認済み。
#[derive(Debug, Clone)]
pub struct Bundle {
    /// 同梱 lua ツリーの根。`init.lua` と `lua/anvi/` を含む（`NvimConfig::runtime_dir`）。
    pub runtime_dir: PathBuf,
    pub nvim_exe: PathBuf,
}

/// 実行ファイルの隣から同梱物を解決する。
pub fn resolve() -> anyhow::Result<Bundle> {
    let exe = std::env::current_exe().context("current_exe() failed")?;
    let root = exe
        .parent()
        .with_context(|| format!("executable path has no parent: {}", exe.display()))?;

    resolve_at(root)
}

fn resolve_at(root: &Path) -> anyhow::Result<Bundle> {
    let runtime_dir = root.join("runtime");
    // 空ファイルも弾く。0 バイトの init.lua は nvim が黙って無視するため、
    // 「起動はするが契約だけ死ぬ」という最悪の壊れ方をする。
    require_lua(&runtime_dir.join("init.lua"))?;
    require_lua(&runtime_dir.join("lua").join("anvi").join("init.lua"))?;
    require_lua(&runtime_dir.join("lua").join("anvi").join("history.lua"))?;

    let nvim_exe = root.join("nvim").join("bin").join("nvim.exe");
    require_file(&nvim_exe)?;
    // nvim は exe の隣（../share/nvim/runtime）から自分の runtime を探す。無いと
    // syntax.vim すら開けず、原因が分かりにくい E484 だけが出る。
    require_dir(&root.join("nvim").join("share").join("nvim").join("runtime"))?;

    Ok(Bundle {
        runtime_dir,
        nvim_exe,
    })
}

fn require_file(path: &Path) -> anyhow::Result<()> {
    if !path.is_file() {
        bail!("bundled file is missing: {}", path.display());
    }
    Ok(())
}

fn require_dir(path: &Path) -> anyhow::Result<()> {
    if !path.is_dir() {
        bail!("bundled directory is missing: {}", path.display());
    }
    Ok(())
}

fn require_lua(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path).with_context(|| {
        format!(
            "bundled lua file is missing: {} (rebuild the bundle with scripts/make-bundle.sh)",
            path.display()
        )
    })?;
    if !meta.is_file() {
        bail!("bundled lua path is not a file: {}", path.display());
    }
    if meta.len() == 0 {
        bail!(
            "bundled lua file is empty: {} (copy it from crates/anvi-core/runtime/)",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_requires_the_history_module_before_starting_nvim() {
        let root = std::env::temp_dir().join(format!("anvi-bundle-history-{}", std::process::id()));
        let lua = root.join("runtime/lua/anvi");
        std::fs::create_dir_all(&lua).unwrap();
        std::fs::create_dir_all(root.join("nvim/bin")).unwrap();
        std::fs::create_dir_all(root.join("nvim/share/nvim/runtime")).unwrap();
        std::fs::write(root.join("runtime/init.lua"), "-- entry").unwrap();
        std::fs::write(lua.join("init.lua"), "-- core").unwrap();
        std::fs::write(root.join("nvim/bin/nvim.exe"), "test placeholder").unwrap();
        let history = lua.join("history.lua");
        if history.exists() {
            std::fs::remove_file(&history).unwrap();
        }
        let err = resolve_at(&root).unwrap_err().to_string();
        assert!(err.contains("history.lua"), "{err}");
        assert!(err.contains("make-bundle.sh"), "{err}");
        std::fs::write(&history, "").unwrap();
        assert!(resolve_at(&root).unwrap_err().to_string().contains("empty"));
        std::fs::write(&history, "return {}").unwrap();
        assert!(resolve_at(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}

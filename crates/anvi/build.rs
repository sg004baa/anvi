//! exe に Windows リソース（アイコンとバージョン情報）を埋め込む。
//!
//! アイコンの名前 ID は `1`（`winresource::WindowsResource::set_icon` の既定）。
//! **トレイはこの埋め込みアイコンを `Icon::from_resource(1, ..)` で読む**ので、
//! ここを消すとトレイアイコンも出なくなる。絵の出典は `scripts/make-icon.py`。
//!
//! このクレートは Windows 専用（`main.rs` の `compile_error!`）なので、
//! ターゲットによる分岐は置かない。

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let icon = manifest_dir.join("../../assets/anvi.ico");
    let icon = icon.to_str().expect("the icon path is not UTF-8");

    winresource::WindowsResource::new()
        .set_icon(icon)
        .set("ProductName", "anvi")
        .set("FileDescription", "anvi - edit any input field in Neovim")
        .set("OriginalFilename", "anvi.exe")
        .set("CompanyName", "sg004baa")
        .set("LegalCopyright", "Copyright (c) 2026 sg004baa")
        .compile()
        .expect("failed to embed the Windows resources");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={icon}");
}

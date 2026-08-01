//! フォントファミリの解決先（同梱フォント + システムフォント）。
//!
//! 同梱フォント（`assets/fonts/MoralerspaceArgonHW-Regular.ttf`）は exe に焼き込み、
//! DirectWrite のカスタムコレクションとして持つ。**利用者の環境に何も入れさせずに
//! 等幅 + 日本語を保証する**ためで、これが `FontSpec` の既定であり鎖の最後尾でもある
//! （→ `gui::font`）。
//!
//! ファミリ名は「同梱 → システム」の順に引く。`guifont` でシステムフォントを
//! 指定した場合はそちらが primary になり、同梱フォントは鎖の最後尾として残る。

use anyhow::{Context as _, bail};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteFactory2, IDWriteFactory5, IDWriteFontCollection, IDWriteFontSetBuilder1,
};
use windows::core::{BOOL, Interface as _, PCWSTR};

use crate::gui::font::BUNDLED_FAMILY;

/// 同梱フォントの実体。`include_bytes!` なので `'static`、つまり
/// `CreateInMemoryFontFileReference` に渡したポインタは永遠に有効。
static BUNDLED_TTF: &[u8] =
    include_bytes!("../../../../assets/fonts/MoralerspaceArgonHW-Regular.ttf");

/// ファミリ名を引ける 2 つのコレクション。
pub struct Fonts {
    bundled: IDWriteFontCollection,
    system: IDWriteFontCollection,
}

impl Fonts {
    pub fn new(dwrite: &IDWriteFactory2) -> anyhow::Result<Self> {
        let bundled = bundled_collection(dwrite)?;
        let mut system: Option<IDWriteFontCollection> = None;
        // SAFETY: 出力先はローカル変数。checkforupdates は false（毎回の再走査は不要）。
        unsafe { dwrite.GetSystemFontCollection(&mut system, false) }
            .context("GetSystemFontCollection failed")?;
        let system = system.context("the system font collection is unavailable")?;

        let fonts = Self { bundled, system };
        // 同梱フォントの name テーブルとコードの文字列がずれていたら、鎖の最後尾が
        // 消えて CJK が化ける。起動時に落として気づけるようにする。
        if fonts.find(BUNDLED_FAMILY).is_none() {
            bail!("the bundled font does not expose the family {BUNDLED_FAMILY:?}");
        }
        Ok(fonts)
    }

    /// ファミリ名を解決する。同梱を先に見るので、同名のフォントが入っていても
    /// 同梱側が勝つ（同梱の版で寸法を測ったのに描画は別版、という食い違いを防ぐ）。
    pub fn find(&self, family: &str) -> Option<(&IDWriteFontCollection, u32)> {
        for collection in [&self.bundled, &self.system] {
            if let Some(index) = find_family(collection, family) {
                return Some((collection, index));
            }
        }
        None
    }
}

fn find_family(collection: &IDWriteFontCollection, family: &str) -> Option<u32> {
    let name: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
    let mut index = 0u32;
    let mut exists = BOOL(0);
    // SAFETY: name は NUL 終端の生存スライス。index と exists はローカル変数。
    unsafe { collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists) }.ok()?;
    exists.as_bool().then_some(index)
}

/// 同梱 ttf 1 本だけを含むコレクションを作る。
///
/// ローダは登録したまま解除しない。コレクションが参照しているものを外す手段が
/// 無く、プロセスの寿命と同じだけ生かすのが唯一安全な扱いだからである。
fn bundled_collection(dwrite: &IDWriteFactory2) -> anyhow::Result<IDWriteFontCollection> {
    let factory: IDWriteFactory5 = dwrite
        .cast()
        .context("IDWriteFactory5 is unavailable on this system")?;

    // SAFETY: すべて直前に作った COM 参照へのメソッド呼び出し。フォントデータは
    // `'static` なので、ローダが後から読みに来ても生きている。
    unsafe {
        let loader = factory
            .CreateInMemoryFontFileLoader()
            .context("CreateInMemoryFontFileLoader failed")?;
        factory
            .RegisterFontFileLoader(&loader)
            .context("RegisterFontFileLoader failed")?;

        let file = loader
            .CreateInMemoryFontFileReference(
                &factory,
                BUNDLED_TTF.as_ptr().cast(),
                u32::try_from(BUNDLED_TTF.len()).context("the bundled font is too large")?,
                None,
            )
            .context("CreateInMemoryFontFileReference failed")?;

        let builder: IDWriteFontSetBuilder1 = factory
            .CreateFontSetBuilder()
            .context("CreateFontSetBuilder failed")?;
        builder.AddFontFile(&file).context("AddFontFile failed")?;
        let set = builder.CreateFontSet().context("CreateFontSet failed")?;

        Ok(factory
            .CreateFontCollectionFromFontSet(&set)
            .context("CreateFontCollectionFromFontSet failed")?
            .into())
    }
}

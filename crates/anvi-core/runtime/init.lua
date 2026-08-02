-- 同梱 init.lua（エントリポイント）。`-u` で指定される。
-- 読み込み順の全てをここで決める（DESIGN §4.7 / §5.4）。

-- 同梱ディレクトリは prepend（ローカル側に lua/anvi/ があっても隠蔽されないよう先に置く）
local bundle = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
vim.opt.runtimepath:prepend(bundle)

local aw = require("anvi")

-- 1. 同梱コア。契約を確立する
aw.setup()

-- 2. ローカル設定（任意）。壊れていても起動を止めない。
--    場所を決めるのは nvim であってこのアプリではない。`NVIM_APPNAME=anvi` なので
--    `$XDG_CONFIG_HOME/anvi`、XDG_CONFIG_HOME が無ければ `%LOCALAPPDATA%\anvi`。
--    「効かない」を勘で探させないため、解決先は必ず host のログへ流す
local cfg_dir = vim.fn.stdpath("config")
local cfg_file = vim.fs.joinpath(cfg_dir, "init.lua")
local found = vim.uv.fs_stat(cfg_file) ~= nil

aw.report_config(cfg_dir, found)

if found then
  vim.opt.runtimepath:append(cfg_dir) -- ローカル側は append
  local ok, err = pcall(dofile, cfg_file)
  if not ok then
    -- 起動は続行する。原因が分かるよう host には必ず伝える
    aw.report_error("user_config_error", tostring(err))
  end
end

-- 3. 契約の再宣言。ローカル設定に潰されていても戻す
aw.enforce_contract()

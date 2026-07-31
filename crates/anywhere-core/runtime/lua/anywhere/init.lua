-- 同梱コア（DESIGN 付録 A-2）。
-- 持つのは host との RPC、セッションバッファ、`:w` / `:q` 系の乗っ取り、
-- 異常終了通知だけ。見た目やキーマップはローカル設定の領分（DESIGN §5.4）。

local M = {
  host = nil, -- host の RPC チャンネル ID
  buf = nil, -- 現在のセッションバッファ
}

local function notify(event, payload)
  if M.host then
    vim.rpcnotify(M.host, event, payload)
  end
end

--- 起動時のエラー報告。この時点ではまだ host が接続していないので溜めておく
M.pending_errors = {}
function M.report_error(kind, msg)
  table.insert(M.pending_errors, { kind = kind, message = msg })
end

--- host が接続時に呼ぶ
function M.set_host(chan)
  M.host = chan
  for _, e in ipairs(M.pending_errors) do
    notify("init_error", e) -- host 側でログに出す
  end
  M.pending_errors = {}
end

--- host がセッション開始時に呼ぶ
function M.start_session(lines, filetype)
  -- 古いバッファを破棄（DESIGN §4.1 / §6.4）
  if M.buf and vim.api.nvim_buf_is_valid(M.buf) then
    vim.api.nvim_buf_delete(M.buf, { force = true })
  end
  M.buf = nil

  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(buf, "anywhere://edit")
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].buftype = "acwrite" -- ディスクには書かない（DESIGN §4.5）
  vim.bo[buf].filetype = filetype or ""

  -- :w を乗っ取る。ディスクには書かない
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    buffer = buf,
    callback = function()
      notify("session_write", vim.api.nvim_buf_get_lines(buf, 0, -1, false))
      vim.bo[buf].modified = false -- acwrite では自分で降ろす必要がある
    end,
  })

  vim.api.nvim_set_current_buf(buf)
  vim.bo[buf].modified = false
  M.buf = buf
  return buf
end

--- セッション終了（nvim は死なない）。反映するかは host が「保存を受信したか」で決める
local function finish()
  notify("session_end", nil)
end

--- 同梱コア。ローカル設定より前に一度だけ呼ぶ
function M.setup()
  -- bang = true は必須。`:q!` は abbrev 展開で「AwQuit!」の形になるため
  vim.api.nvim_create_user_command("AwWriteQuit", function()
    vim.cmd("write") -- BufWriteCmd 経由で内容が host に渡る
    finish()
  end, { bang = true })

  vim.api.nvim_create_user_command("AwQuit", function()
    finish()
  end, { bang = true })

  -- 見た目やオプションはここに入れない。ローカル設定の領分（DESIGN §5.4）
end

--- 契約の再宣言。ローカル設定を読み込んだ後に呼ぶ（DESIGN §4.7）
--- 順序に対して脆いのはここに集約されている 3 つだけ
function M.enforce_contract()
  -- 1. キーマップ
  vim.keymap.set("n", "ZZ", "<Cmd>AwWriteQuit<CR>")
  vim.keymap.set("n", "ZQ", "<Cmd>AwQuit<CR>")

  -- 2. :q 系の乗っ取り（網羅的ではない。抜けたら追加する）
  local function abbr(lhs, rhs)
    vim.cmd(([[cnoreabbrev <expr> %s (getcmdtype()==#':' && getcmdline()==#%q) ? %q : %q]]):format(lhs, lhs, rhs, lhs))
  end

  abbr("q", "AwQuit")
  abbr("q!", "AwQuit")
  abbr("wq", "AwWriteQuit")
  abbr("x", "AwWriteQuit")

  -- 3. 安全網: 乗っ取りを抜けて本当に終了しようとした場合
  --    clear = true で二重登録を防ぐ（再宣言されうるため）
  local grp = vim.api.nvim_create_augroup("AnywhereContract", { clear = true })
  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = grp,
    callback = function()
      notify("nvim_dying", nil)
    end,
  })
end

return M

-- Display-only history. The authoritative five snapshots live in the Rust host.
local M = {}
local entries = {}
local state
local selection_ns = vim.api.nvim_create_namespace("AnviHistorySelection")

local function mark_selection()
  if not state or state.body then return end
  vim.api.nvim_buf_clear_namespace(state.buf, selection_ns, 0, -1)
  if #entries == 0 then return end
  local row = vim.api.nvim_win_get_cursor(state.win)[1]
  if vim.api.nvim_win_get_cursor(state.win)[2] ~= 0 then
    vim.api.nvim_win_set_cursor(state.win, { row, 0 })
  end
  state.index = row
  vim.api.nvim_buf_set_extmark(state.buf, selection_ns, row - 1, 0, {
    virt_text = { { ">", "CursorLineNr" } },
    virt_text_pos = "overlay",
  })
end

local function config(body)
  local width = math.max(1, math.min(80, vim.o.columns - 4))
  local available = math.max(1, vim.o.lines - vim.o.cmdheight - 4)
  local height = math.min(available, body and 16 or math.max(1, #entries))
  return {
    relative = "editor",
    row = math.max(0, math.floor((available - height) / 2)),
    col = math.max(0, math.floor((vim.o.columns - width) / 2)),
    width = width,
    height = height,
    style = "minimal",
    border = "single",
  }
end

function M.close()
  local old = state
  state = nil
  if not old then return end
  vim.o.ruler = old.ruler
  vim.o.guicursor = old.guicursor
  if vim.api.nvim_win_is_valid(old.win) then
    vim.api.nvim_win_close(old.win, true)
  end
  if vim.api.nvim_buf_is_valid(old.buf) then
    vim.api.nvim_buf_delete(old.buf, { force = true })
  end
  if vim.api.nvim_win_is_valid(old.origin) then
    vim.api.nvim_set_current_win(old.origin)
  end
end

local list_keys = { "j", "k", "<Down>", "<Up>", "h", "l", "<Left>", "<Right>",
  "w", "b", "e", "0", "$", "i", "a", "I", "A", "o", "O", "v", "V", "<C-v>" }

local function list_cursor()
  if state and not state.body and vim.api.nvim_get_current_win() == state.win then
    vim.api.nvim_set_hl(0, "AnviHistoryHiddenCursor", { fg = "#000000", bg = "#000000", blend = 100 })
    vim.o.guicursor = "a:block-AnviHistoryHiddenCursor"
  end
end

local function configure_navigation(body)
  for _, key in ipairs(list_keys) do
    if body then
      pcall(vim.keymap.del, "n", key, { buffer = state.buf })
    else
      vim.keymap.set("n", key, function()
        local delta = (key == "j" or key == "<Down>") and 1
          or ((key == "k" or key == "<Up>") and -1 or 0)
        local row = vim.api.nvim_win_get_cursor(state.win)[1]
        row = math.max(1, math.min(math.max(1, #entries), row + delta * vim.v.count1))
        vim.api.nvim_win_set_cursor(state.win, { row, 0 })
        mark_selection()
      end, { buffer = state.buf, silent = true, nowait = true })
    end
  end
  if body then vim.o.guicursor = state.guicursor else list_cursor() end
end

local function show(body)
  state.body = body
  vim.api.nvim_buf_clear_namespace(state.buf, selection_ns, 0, -1)
  local lines = {}
  if body then
    lines = entries[state.index]
  elseif #entries == 0 then
    lines = { "[保存履歴なし]" }
  else
    for index, saved in ipairs(entries) do
      local first = saved[1] or ""
      if first == "" then
        first = (#saved <= 1) and "[空の保存内容]" or "[空の先頭行]"
      end
      table.insert(lines, tostring(index) .. "  " .. first)
    end
  end
  vim.bo[state.buf].readonly = false
  vim.bo[state.buf].modifiable = true
  vim.api.nvim_buf_set_lines(state.buf, 0, -1, false, lines)
  vim.bo[state.buf].modified = false
  vim.bo[state.buf].modifiable = false
  vim.bo[state.buf].readonly = true
  vim.api.nvim_win_set_config(state.win, config(body))
  vim.wo[state.win].cursorline = not body
  vim.wo[state.win].wrap = body
  vim.api.nvim_win_set_cursor(state.win, { body and 1 or state.index, 0 })
  configure_navigation(body)
  mark_selection()
end

-- Called by both buffer-local mappings and anvi's session-end commands.
-- Returning true means this action belongs to history, never to the edit session.
function M.back()
  if not state then return false end
  if state.body then show(false) else M.close() end
  return true
end

function M.set(saved)
  M.close()
  entries = vim.deepcopy(saved)
end

function M.open()
  if state then
    vim.api.nvim_set_current_win(state.win)
    return
  end
  local origin = vim.api.nvim_get_current_win()
  local buf = vim.api.nvim_create_buf(false, true)
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].swapfile = false
  vim.bo[buf].undofile = false
  vim.bo[buf].undolevels = -1
  local win = vim.api.nvim_open_win(buf, true, config(false))
  state = { origin = origin, buf = buf, win = win, index = 1, body = false, ruler = vim.o.ruler, guicursor = vim.o.guicursor }
  -- With laststatus=2, Neovim prints a second position readout for floats
  -- on the command line. Keep the original statusline, hide only that ruler.
  vim.o.ruler = false
  local function map(key, callback)
    vim.keymap.set("n", key, callback, { buffer = buf, silent = true, nowait = true })
  end
  map("q", M.back)
  map("<Esc>", M.back)
  map("<CR>", function()
    if state and not state.body and #entries > 0 then
      state.index = vim.api.nvim_win_get_cursor(win)[1]
      show(true)
    end
  end)
  -- Native motions/yank and the user's clipboard setting are left untouched.
  local group = vim.api.nvim_create_augroup("AnviHistory", { clear = true })
  vim.api.nvim_create_autocmd("WinLeave", {
    group = group, buffer = buf,
    callback = function() if state then vim.o.guicursor = state.guicursor end end,
  })
  vim.api.nvim_create_autocmd("WinEnter", {
    group = group, buffer = buf, callback = list_cursor,
  })
  vim.api.nvim_create_autocmd("ColorScheme", {
    group = group, callback = list_cursor,
  })
  vim.api.nvim_create_autocmd("CursorMoved", {
    group = group,
    buffer = buf,
    callback = mark_selection,
  })
  vim.api.nvim_create_autocmd("VimResized", {
    group = group,
    callback = function()
      if state then vim.api.nvim_win_set_config(state.win, config(state.body)) end
    end,
  })
  vim.api.nvim_create_autocmd("WinClosed", {
    group = group,
    pattern = tostring(win),
    once = true,
    callback = function()
      if state and state.win == win then
        vim.o.ruler = state.ruler
        vim.o.guicursor = state.guicursor
        state = nil
      end
    end,
  })
  show(false)
end

return M

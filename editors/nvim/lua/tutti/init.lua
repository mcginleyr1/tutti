-- tutti.nvim: Neovim inside a tutti pane. Inert unless $TUTTI_PANE is set.
local M = {
  -- Whether Neovim handles tutti's direct keys (Ctrl+h/j/k/l, Alt+h/j/k/l,
  -- Alt+x): only then does it tell tutti to pass them through.
  keys = false,
  timeout = 500,
}

local wincmd = { left = 'h', down = 'j', up = 'k', right = 'l' }

-- OSC 1337 SetUserVar; an empty value clears the var.
local function user_var(name, value)
  return ('\27]1337;SetUserVar=%s=%s\7'):format(name, vim.base64.encode(value))
end

-- Writes to the terminal tutti reads: through the TUI since 0.12, in order with
-- its own output; before that, through the server's stderr.
local function send(data)
  if vim.api.nvim_ui_send then
    vim.api.nvim_ui_send(data)
  else
    vim.api.nvim_chan_send(vim.v.stderr, data)
  end
end

function M.announce(on)
  send(
    user_var('TUTTI_KEYS', on and M.keys and 'pane' or '')
      .. user_var('TUTTI_EDITOR', on and 'nvim ' .. vim.v.servername or '')
  )
end

-- Runs `tutti <args>`, output discarded, and returns its exit code: -1 when it
-- could not run, 124 when it outlived the timeout. Never throws.
function M.run(args)
  local ok, result = pcall(function()
    local opts = { timeout = M.timeout, stdout = false, stderr = false }
    return vim.system(vim.list_extend({ 'tutti' }, args), opts):wait()
  end)
  return ok and result and result.code or -1
end

-- Focuses the tutti pane beyond this one; true when focus moved.
function M.focus(direction, wrap)
  local args = { 'pane', 'focus', '--direction', direction, '--from', vim.env.TUTTI_PANE }
  return M.run(wrap and vim.list_extend(args, { '--wrap' }) or args) == 0
end

function M.resize(direction, amount)
  local args = { 'pane', 'resize', '--direction', direction, '--from', vim.env.TUTTI_PANE }
  return M.run(amount and vim.list_extend(args, { '--amount', tostring(amount) }) or args) == 0
end

-- Moves to the Neovim window in `direction`, or past Neovim's edge to the
-- tutti pane there.
function M.navigate(direction)
  assert(wincmd[direction], 'tutti: direction must be left, right, up or down')
  local win = vim.api.nvim_get_current_win()
  vim.cmd.wincmd(wincmd[direction])
  if vim.api.nvim_get_current_win() == win then
    M.focus(direction)
  end
end

-- Registers the announcements (idempotent); announces now when VimEnter has
-- already fired, as it has for a lazy-loaded plugin.
function M.activate()
  local group = vim.api.nvim_create_augroup('tutti', {})
  vim.api.nvim_create_autocmd({ 'VimEnter', 'VimResume' }, {
    group = group,
    callback = function()
      M.announce(true)
    end,
  })
  vim.api.nvim_create_autocmd({ 'VimLeavePre', 'VimSuspend' }, {
    group = group,
    callback = function()
      M.announce(false)
    end,
  })
  if vim.v.vim_did_enter == 1 then
    M.announce(true)
  end
end

-- Picks an agent from the session's first `tutti watch` snapshot and focuses
-- it.
function M.agents()
  local buffered, line = '', nil
  local proc
  proc = vim.system({ 'tutti', 'watch' }, {
    timeout = 5000,
    stdout = function(_, data)
      buffered = buffered .. (data or '')
      if line or not buffered:find('\n') then
        return
      end
      line = buffered:match('^[^\n]*')
      pcall(proc.kill, proc, 'sigterm')
      vim.schedule(function()
        local agents = vim.tbl_filter(function(pane)
          return pane.agent ~= nil
        end, vim.json.decode(line, { luanil = { object = true } }).panes)
        if #agents == 0 then
          return vim.notify('tutti: no agents in this session')
        end
        vim.ui.select(agents, {
          prompt = 'tutti agents',
          format_item = function(pane)
            return ('%-8s %s · %s'):format(pane.state, pane.title, pane.workspace)
          end,
        }, function(pane)
          if pane and M.run({ 'pane', 'focus', tostring(pane.id) }) ~= 0 then
            vim.notify('tutti: could not focus pane ' .. pane.id, vim.log.levels.ERROR)
          end
        end)
      end)
    end,
  }, function(done)
    if not line then
      local reason = vim.trim(done.stderr or '')
      vim.schedule(function()
        vim.notify(reason ~= '' and reason or 'tutti watch: no snapshot', vim.log.levels.ERROR)
      end)
    end
  end)
end

function M.setup(opts)
  if not vim.env.TUTTI_PANE or not (opts or {}).keys then
    return
  end
  M.keys = true
  for direction, key in pairs(wincmd) do
    vim.keymap.set('n', '<C-' .. key .. '>', function()
      M.navigate(direction)
    end, { desc = 'tutti: focus ' .. direction })
    vim.keymap.set('n', '<M-' .. key .. '>', function()
      M.resize(direction)
    end, { desc = 'tutti: resize ' .. direction })
  end
  M.activate()
end

return M

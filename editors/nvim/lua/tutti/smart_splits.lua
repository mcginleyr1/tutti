-- A smart-splits.nvim backend (protocol 3.0.0): past Neovim's last window,
-- focus and resize move on to tutti's panes.
local tutti = require('tutti')

local M = { name = 'tutti', protocol_version = '3.0.0' }

function M.detect()
  return vim.env.TUTTI_PANE ~= nil and vim.fn.executable('tutti') == 1
end

function M.move(direction, opts)
  return tutti.focus(direction, (opts or {}).at_edge == 'wrap')
end

-- smart-splits counts `opts.amount` in cells and tutti in split ratio, with
-- no split size to convert by, so a count takes that many of tutti's own
-- Alt+h/j/k/l steps (0.05) instead.
function M.resize(direction)
  return tutti.resize(direction, vim.v.count1 > 1 and 0.05 * vim.v.count1 or nil)
end

-- smart-splits maps the keys, so tutti can hand them to Neovim.
function M.activate()
  tutti.keys = true
  tutti.activate()
end

return M

-- Headless tests for tutti.nvim against the stub editors/tests/tutti:
--   nvim --headless -u NONE -l <abs path>/editors/nvim/tests/run.lua
local plugin = vim.fn.fnamemodify(arg[0], ':p:h:h')
vim.opt.rtp:prepend(plugin)
local stub = vim.fn.tempname()
vim.env.TUTTI_STUB = stub
vim.env.PATH = vim.fn.fnamemodify(plugin, ':h') .. '/tests:' .. vim.env.PATH
vim.env.TUTTI_PANE = nil

local failed = 0
local function test(name, fn)
  vim.fn.delete(stub, 'rf')
  vim.fn.mkdir(stub, 'p')
  vim.cmd('silent! only')
  local ok, err = pcall(fn)
  io.stdout:write((ok and 'ok   ' or 'FAIL ') .. name .. (ok and '' or '\n     ' .. tostring(err)) .. '\n')
  failed = failed + (ok and 0 or 1)
end

local function eq(expected, got)
  if not vim.deep_equal(expected, got) then
    error(('expected %s, got %s'):format(vim.inspect(expected), vim.inspect(got)), 2)
  end
end

-- How the stub answers its nth call: { out =, err =, sleep =, exit = }.
local function answer(n, files)
  for name, value in pairs(files) do
    vim.fn.writefile({ tostring(value) }, ('%s/%d.%s'):format(stub, n, name))
  end
end

local function calls()
  return vim.fn.filereadable(stub .. '/log') == 1 and vim.fn.readfile(stub .. '/log') or {}
end

local sent = {}
vim.api.nvim_ui_send = function(data)
  table.insert(sent, data)
end
local function vars(keys, editor)
  local function var(name, value)
    return ('\27]1337;SetUserVar=%s=%s\7'):format(name, vim.base64.encode(value))
  end
  return { var('TUTTI_KEYS', keys) .. var('TUTTI_EDITOR', editor) }
end
local editor = 'nvim ' .. vim.v.servername

local tutti = require('tutti')
local backend = require('tutti.smart_splits')

test('inert without $TUTTI_PANE', function()
  vim.cmd.runtime('plugin/tutti.lua')
  tutti.setup({ keys = true })
  eq(0, vim.fn.exists(':TuttiAgents'))
  eq('', vim.fn.maparg('<C-h>', 'n'))
  eq(false, backend.detect())
  eq({}, sent)
  vim.cmd.vsplit()
  tutti.navigate('left')
  eq({ false, false }, { tutti.focus('left'), tutti.resize('up', 0.1) })
  eq({}, calls())
end)

vim.env.TUTTI_PANE = '7'

test('the plugin entry announces the editor and leaves the keys to tutti', function()
  assert(vim.v.servername ~= '', 'no server name')
  vim.cmd.runtime('plugin/tutti.lua')
  eq(2, vim.fn.exists(':TuttiAgents'))
  sent = {}
  vim.api.nvim_exec_autocmds('VimEnter', {})
  eq(vars('', editor), sent)
end)

test('suspend and leave clear the vars, resume sets them again', function()
  sent = {}
  vim.api.nvim_exec_autocmds('VimSuspend', {})
  vim.api.nvim_exec_autocmds('VimResume', {})
  vim.api.nvim_exec_autocmds('VimLeavePre', {})
  eq({ vars('', '')[1], vars('', editor)[1], vars('', '')[1] }, sent)
end)

test('navigate moves between windows, then past the edge to tutti', function()
  vim.cmd.vsplit()
  local left = vim.api.nvim_get_current_win()
  tutti.navigate('right')
  assert(vim.api.nvim_get_current_win() ~= left, 'stayed in the left window')
  eq({}, calls())
  tutti.navigate('right')
  eq({ '[pane][focus][--direction][right][--from][7]' }, calls())
  eq(false, pcall(tutti.navigate, 'north'))
end)

test('backend: protocol fields and detect', function()
  eq({ 'tutti', '3.0.0' }, { backend.name, backend.protocol_version })
  eq(true, backend.detect())
end)

test('backend: move is true iff tutti moved focus', function()
  answer(2, { exit = 1 })
  eq(true, backend.move('left', {}))
  eq(false, backend.move('down', { at_edge = 'stop' }))
  eq(true, backend.move('up', { at_edge = 'wrap' }))
  eq(true, backend.move('right'))
  eq({
    '[pane][focus][--direction][left][--from][7]',
    '[pane][focus][--direction][down][--from][7]',
    '[pane][focus][--direction][up][--from][7][--wrap]',
    '[pane][focus][--direction][right][--from][7]',
  }, calls())
end)

test('backend: move gives up at the timeout', function()
  answer(1, { sleep = 3 })
  tutti.timeout = 100
  local start = vim.uv.hrtime()
  local moved = backend.move('left', {})
  tutti.timeout = 500
  eq(false, moved)
  assert((vim.uv.hrtime() - start) / 1e6 < 1000, 'waited past the timeout')
end)

test('backend: move without tutti to run is false, not an error', function()
  local path = vim.env.PATH
  vim.env.PATH = '/nonexistent'
  local ok, moved = pcall(backend.move, 'left', {})
  vim.env.PATH = path
  eq({ true, false }, { ok, moved })
end)

test("backend: resize takes tutti's steps, a count's worth, never cells", function()
  answer(2, { exit = 1 })
  eq(true, backend.resize('left', { amount = 3 }))
  eq(false, backend.resize('down', {}))
  vim.keymap.set('n', '<F5>', function()
    backend.resize('right', { amount = 9 })
  end)
  vim.api.nvim_feedkeys(vim.keycode('3<F5>'), 'x', false)
  eq(true, tutti.resize('up', 0.1))
  eq({
    '[pane][resize][--direction][left][--from][7]',
    '[pane][resize][--direction][down][--from][7]',
    '[pane][resize][--direction][right][--from][7][--amount][0.15]',
    '[pane][resize][--direction][up][--from][7][--amount][0.1]',
  }, calls())
end)

test('backend: activate claims the keys', function()
  backend.activate()
  sent = {}
  vim.api.nvim_exec_autocmds('VimResume', {})
  eq(vars('pane', editor), sent)
  tutti.keys = false
end)

test('setup({ keys = true }) maps the keys and claims them', function()
  tutti.setup({ keys = true })
  sent = {}
  vim.api.nvim_exec_autocmds('VimResume', {})
  eq(vars('pane', editor), sent)
  vim.cmd.vsplit()
  vim.api.nvim_feedkeys(vim.keycode('<C-l><C-l><C-h><C-h><C-j><M-k>'), 'x', false)
  eq({
    '[pane][focus][--direction][right][--from][7]',
    '[pane][focus][--direction][left][--from][7]',
    '[pane][focus][--direction][down][--from][7]',
    '[pane][resize][--direction][up][--from][7]',
  }, calls())
end)

test(':TuttiAgents picks an agent from the first watch snapshot', function()
  answer(1, {
    out = '{"type":"snapshot","session":"tutti","wire_rev":9,"attached_terminal":null,"panes":['
      .. '{"id":3,"workspace":"api","dir":"/w/api","tab":"main","title":"claude","agent":"claude","state":"blocked","source":"hook","exited":null,"focused":false},'
      .. '{"id":4,"workspace":"web","dir":"/w/web","tab":"main","title":"zsh","agent":null,"state":"unknown","source":null,"exited":null,"focused":true},'
      .. '{"id":5,"workspace":"web","dir":"/w/web","tab":"logs","title":"codex","agent":"codex","state":"working","source":"screen","exited":null,"focused":false}]}',
    sleep = 3,
  })
  local shown, choose
  vim.ui.select = function(items, opts, on_choice)
    shown = vim.tbl_map(opts.format_item, items)
    choose = function(i)
      on_choice(items[i], i)
    end
  end
  vim.cmd.TuttiAgents()
  assert(
    vim.wait(3000, function()
      return shown ~= nil
    end),
    'no picker'
  )
  eq({ 'blocked  claude · api', 'working  codex · web' }, shown)
  choose(2)
  eq({ '[watch]', '[pane][focus][5]' }, calls())
end)

test(':TuttiAgents says when there are no agents', function()
  answer(1, { out = '{"type":"snapshot","session":"tutti","wire_rev":9,"attached_terminal":null,"panes":[]}' })
  local notes = {}
  vim.notify = function(message)
    table.insert(notes, message)
  end
  vim.cmd.TuttiAgents()
  assert(
    vim.wait(3000, function()
      return #notes > 0
    end),
    'no notification'
  )
  eq({ 'tutti: no agents in this session' }, notes)
end)

test(':TuttiAgents reports a watch that fails', function()
  answer(1, { err = "tutti watch: no tutti-server is running for session 'tutti'", exit = 1 })
  local notes = {}
  vim.notify = function(message, level)
    table.insert(notes, { message, level })
  end
  vim.cmd.TuttiAgents()
  assert(
    vim.wait(3000, function()
      return #notes > 0
    end),
    'no notification'
  )
  eq({ { "tutti watch: no tutti-server is running for session 'tutti'", vim.log.levels.ERROR } }, notes)
end)

if failed > 0 then
  io.stdout:write(failed .. ' failed\n')
  os.exit(1)
end

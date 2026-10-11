if vim.g.loaded_tutti or not vim.env.TUTTI_PANE then
  return
end
vim.g.loaded_tutti = true

require('tutti').activate()
vim.api.nvim_create_user_command('TuttiAgents', function()
  require('tutti').agents()
end, { desc = 'Pick a tutti agent and focus its pane' })

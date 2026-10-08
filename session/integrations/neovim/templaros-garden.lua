-- Optional integration: :luafile /usr/share/templaros/integrations/neovim/templaros-garden.lua
-- Never edits the user's configuration; jobstart inherits this editor's verified cgroup.
local function report()
  vim.fn.jobstart({ 'templar', 'garden', 'observe', 'project-entered', vim.fn.getcwd(),
    '--role', 'code-editor' }, {
    on_exit = function(_, code)
      if code ~= 0 then
        vim.schedule(function()
          vim.notify('Garden workspace attribution requires a verified local launcher instance and sessiond', vim.log.levels.DEBUG)
        end)
      end
    end,
  })
end
local group = vim.api.nvim_create_augroup('TemplarOSGarden', { clear = true })
vim.api.nvim_create_autocmd('DirChanged', { group = group, callback = report })
report()

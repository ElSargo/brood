local MAX_JSON_BYTES = 8 * 1024
local MAX_TEXT_BYTES = 8 * 1024
local MAX_TARGET_BYTES = 128
local OSC_PREFIX = "\27]777;brood;"
local OSC_SUFFIX = "\7"

local function fail(message)
  vim.notify(message, vim.log.levels.ERROR, { title = "Brood" })
end

local function hex_encode(value)
  return (value:gsub(".", function(byte)
    return string.format("%02x", string.byte(byte))
  end))
end

local function emit(message)
  local ok, payload = pcall(vim.json.encode, message)
  if not ok then
    fail("Could not encode Brood message: " .. tostring(payload))
    return false
  end
  if #payload > MAX_JSON_BYTES then
    fail("Brood message is too large")
    return false
  end

  local frame = OSC_PREFIX .. hex_encode(payload) .. OSC_SUFFIX
  local sent, result = pcall(vim.fn.chansend, vim.v.stderr, frame)
  if not sent or result == 0 then
    fail("Brood control channel is unavailable")
    return false
  end
  return true
end

local function bounded_text(kind, value)
  if value:find("\0", 1, true) then
    fail(kind .. " cannot contain NUL")
    return nil
  end
  if #value > MAX_TEXT_BYTES then
    fail(kind .. " is too large")
    return nil
  end
  return value
end

local function valid_target(target)
  if #target == 0 or #target > MAX_TARGET_BYTES then
    return false
  end

  local actor = target
  if target:sub(1, 8) == "session:" then
    actor = target:sub(9)
  elseif target:sub(1, 13) == "presentation:" then
    actor = target:sub(14)
  elseif target:find(":", 1, true) then
    return false
  end

  return actor ~= ""
    and actor ~= "host"
    and actor:match("^[A-Za-z0-9._-]+$") ~= nil
end

local function agent(prompt)
  prompt = bounded_text("Agent prompt", prompt)
  if prompt and prompt:match("%S") then
    emit({ action = "agent", prompt = prompt })
  elseif prompt then
    fail("Agent prompt cannot be empty")
  end
end

vim.brood = vim.brood or {}
vim.brood.send = emit

vim.api.nvim_create_user_command("BroodDiff", function()
  emit({ action = "diff" })
end, {
  desc = "Open or focus the Brood diff viewer",
})

vim.api.nvim_create_user_command("BroodTerminal", function(options)
  local command = bounded_text("Terminal command", options.args)
  if command then
    emit({ action = "terminal", command = command })
  end
end, {
  nargs = "*",
  desc = "Open a Brood terminal, optionally running a command",
})

vim.api.nvim_create_user_command("BroodAgent", function(options)
  if options.args:match("%S") then
    agent(options.args)
    return
  end
  vim.ui.input({ prompt = "Brood agent prompt: " }, function(prompt)
    if prompt ~= nil then
      agent(prompt)
    end
  end)
end, {
  nargs = "*",
  desc = "Send work to a Brood coding agent",
})

vim.api.nvim_create_user_command("BroodMessage", function(options)
  local target, source = options.args:match("^(%S+)%s+(.+)$")
  if not target or not valid_target(target) then
    fail("Target must be an actor name, session:name, or presentation:name")
    return
  end
  source = bounded_text("Message body", source)
  if not source then
    return
  end

  local decoded, body = pcall(vim.json.decode, source)
  if not decoded then
    body = source
  end
  emit({ action = "message", target = target, body = body })
end, {
  nargs = "+",
  desc = "Send JSON or text to a same-session Brood actor",
})

vim.keymap.set("n", "<Plug>(BroodDiff)", "<Cmd>BroodDiff<CR>", {
  desc = "Brood: open diff viewer",
})
vim.keymap.set("n", "<Plug>(BroodTerminal)", ":<C-U>BroodTerminal ", {
  desc = "Brood: open terminal",
})
vim.keymap.set("n", "<Plug>(BroodAgent)", ":<C-U>BroodAgent ", {
  desc = "Brood: message coding agent",
})
vim.keymap.set("n", "<Plug>(BroodMessage)", ":<C-U>BroodMessage ", {
  desc = "Brood: message actor",
})

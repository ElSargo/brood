local M = {}

local VERSION = 1
local MAX_LINE_BYTES = 12 * 1024
local MAX_INLINE_BYTES = 8 * 1024
local MAX_FILE_BYTES = 8 * 1024 * 1024
local MAX_TRANSFERS = 4
local MAX_PATH_BYTES = 4096
local MAX_ID_BYTES = 128
local MAX_PENDING_EVENTS = 64
local MAX_ACTION_EVENTS = MAX_PENDING_EVENTS - 8
local MAX_DEFERRED_CRITICAL_EVENTS = 64
local MAX_SCREEN_TEXT_BYTES = 9 * 1024
local SAVE_CHUNK_BYTES = 7 * 1024

local inbox_directory = "/brood/io/inbox"
local outbox_directory = "/brood/io/outbox"
local screen_path = "/brood/io/screen.json"
local shutdown_ready_path = "/brood/io/shutdown-ready"

local event_sequence = 0
local save_sequence = 0
local screen_revision = 0
local dirty_screen = true
local installed = false
local started = false
local timer = nil
local presentation_focused = true
local dimensions = { columns = 100, rows = 32 }
local remote_buffers = {}
local open_transfers = {}
local pending_saves = {}
local save_streams = {}
local deferred_critical_events = {}
local critical_write_failure_reported = false

local function read_file(path)
  local file, open_error = io.open(path, "rb")
  if not file then
    return nil, open_error
  end
  local contents = file:read("*a")
  file:close()
  return contents
end

local function write_file(path, contents)
  local file, open_error = io.open(path, "wb")
  if not file then
    return false, open_error
  end
  local ok, write_error = file:write(contents)
  if not ok then
    file:close()
    return false, write_error
  end
  file:flush()
  file:close()
  return true
end

local function sorted_files(directory)
  local files = {}
  local scanner = vim.uv.fs_scandir(directory)
  if not scanner then
    return files
  end
  while true do
    local name, kind = vim.uv.fs_scandir_next(scanner)
    if not name then
      break
    end
    if kind == "file" then
      table.insert(files, name)
    end
  end
  table.sort(files)
  return files
end

local function json_encode(value)
  local ok, encoded = pcall(vim.json.encode, value)
  if not ok then
    return nil, tostring(encoded)
  end
  return encoded
end

local function table_size(value)
  local count = 0
  for _ in pairs(value) do
    count = count + 1
  end
  return count
end

local function write_encoded_event(encoded, limit)
  if #sorted_files(outbox_directory) >= limit then
    return false, "full"
  end
  event_sequence = event_sequence + 1
  local filename = string.format("%012d.json", event_sequence)
  local ok, write_error = write_file(outbox_directory .. "/" .. filename, encoded)
  if not ok then
    return false, tostring(write_error)
  end
  critical_write_failure_reported = false
  return true, nil
end

local function retain_critical_event(encoded)
  if #deferred_critical_events >= MAX_DEFERRED_CRITICAL_EVENTS then
    vim.notify(
      "Brood critical message queue is full",
      vim.log.levels.ERROR
    )
    return false
  end
  table.insert(deferred_critical_events, encoded)
  return true
end

local function flush_critical_events()
  while #deferred_critical_events > 0 do
    local ok, write_error =
      write_encoded_event(deferred_critical_events[1], MAX_PENDING_EVENTS)
    if not ok then
      if write_error ~= "full" and not critical_write_failure_reported then
        critical_write_failure_reported = true
        vim.notify(
          "Brood critical message write failed; retrying: "
            .. tostring(write_error),
          vim.log.levels.ERROR
        )
      end
      return
    end
    table.remove(deferred_critical_events, 1)
  end
end

local function emit_event(message, critical)
  message.v = VERSION
  local encoded, encode_error = json_encode(message)
  if not encoded then
    vim.notify("Brood message encoding failed: " .. encode_error, vim.log.levels.ERROR)
    return false
  end
  if #encoded > MAX_LINE_BYTES then
    vim.notify("Brood message exceeds the JSONL limit", vim.log.levels.ERROR)
    return false
  end

  flush_critical_events()
  if critical == false then
    if #deferred_critical_events > 0 then
      vim.notify(
        "Brood action queue is paused behind critical messages",
        vim.log.levels.WARN
      )
      return false
    end
    local ok, write_error = write_encoded_event(encoded, MAX_ACTION_EVENTS)
    if not ok then
      if write_error == "full" then
        vim.notify("Brood action queue is full", vim.log.levels.WARN)
      else
        vim.notify(
          "Brood action message write failed: " .. tostring(write_error),
          vim.log.levels.ERROR
        )
      end
      return false
    end
    return true
  end

  if #deferred_critical_events > 0 then
    return retain_critical_event(encoded)
  end
  local ok = write_encoded_event(encoded, MAX_PENDING_EVENTS)
  if ok then
    return true
  end
  return retain_critical_event(encoded)
end

local function protocol_error(code, message, request_id)
  emit_event({
    type = "error",
    code = code,
    message = message,
    request_id = request_id,
  })
end

local function bounded_string(value, label, maximum, allow_empty)
  if type(value) ~= "string" then
    error(label .. " must be a string")
  end
  if not allow_empty and #value == 0 then
    error(label .. " cannot be empty")
  end
  if #value > maximum then
    error(label .. " exceeds " .. maximum .. " bytes")
  end
  if value:find("\0", 1, true) then
    error(label .. " cannot contain NUL")
  end
  return value
end

local function request_id(message)
  return bounded_string(message.request_id, "request_id", MAX_ID_BYTES, false)
end

local function utf8_prefix(value, maximum)
  if #value <= maximum then
    return value
  end
  local ending = maximum
  while ending > 0 do
    local next_byte = value:byte(ending + 1)
    if not next_byte or next_byte < 0x80 or next_byte > 0xBF then
      break
    end
    ending = ending - 1
  end
  return value:sub(1, ending)
end

local function utf8_chunk(value, offset, maximum)
  local start_index = offset + 1
  local end_index = math.min(#value, offset + maximum)
  if end_index < #value then
    while end_index >= start_index do
      local next_byte = value:byte(end_index + 1)
      if not next_byte or next_byte < 0x80 or next_byte > 0xBF then
        break
      end
      end_index = end_index - 1
    end
  end
  if end_index < start_index then
    error("could not split UTF-8 save content")
  end
  return value:sub(start_index, end_index)
end

local function mode_name(mode)
  local first = mode:sub(1, 1)
  if first == "i" then
    return "insert"
  elseif first == "R" then
    return "replace"
  elseif first == "v" or first == "V" or first == "\22" then
    return "visual"
  elseif first == "c" then
    return "command"
  elseif first == "t" then
    return "terminal"
  elseif first == "s" or first == "S" or first == "\19" then
    return "select"
  elseif first == "o" then
    return "operator"
  end
  return "normal"
end

local function display_slice(value, left_column, width, byte_budget)
  local character_count = vim.fn.strchars(value)
  local display_column = 0
  local result = {}
  local result_bytes = 0
  for index = 0, character_count - 1 do
    local character = vim.fn.strcharpart(value, index, 1)
    local character_width = math.max(1, vim.fn.strdisplaywidth(character))
    local next_column = display_column + character_width
    if next_column > left_column and display_column < left_column + width then
      if result_bytes + #character > byte_budget then
        break
      end
      table.insert(result, character)
      result_bytes = result_bytes + #character
    end
    display_column = next_column
    if display_column >= left_column + width then
      break
    end
  end
  return table.concat(result)
end

local function current_buffer_state(buffer)
  local metadata = remote_buffers[buffer]
  local path = metadata and metadata.path or vim.api.nvim_buf_get_name(buffer)
  local version = metadata and metadata.version or nil
  return {
    path = utf8_prefix(path, MAX_PATH_BYTES),
    version = version,
    modified = vim.bo[buffer].modified,
    readonly = vim.bo[buffer].readonly,
    line_count = vim.api.nvim_buf_line_count(buffer),
  }
end

local function encode_bounded_screen(message, line_budget)
  local encoded, encode_error = json_encode(message)
  if not encoded then
    return nil, encode_error
  end
  if #encoded <= MAX_LINE_BYTES then
    return encoded, nil
  end

  -- Screen fields share one encoded JSONL budget. Trim low-priority metadata
  -- first, then reduce every rendered row together so escaping cannot make an
  -- otherwise valid state disappear from the client.
  message.message = utf8_prefix(message.message, 256)
  message.buffer.path = utf8_prefix(message.buffer.path, 512)
  if message.cmdline then
    message.cmdline.text = utf8_prefix(message.cmdline.text, 512)
    message.cmdline.cursor = math.min(
      message.cmdline.cursor,
      #message.cmdline.text
    )
  end

  local budget = line_budget
  while true do
    encoded, encode_error = json_encode(message)
    if not encoded then
      return nil, encode_error
    end
    if #encoded <= MAX_LINE_BYTES then
      return encoded, nil
    end
    if budget <= 0 then
      break
    end
    budget = math.floor(budget / 2)
    for index, line in ipairs(message.lines) do
      message.lines[index] = utf8_prefix(line, budget)
    end
  end

  -- Rows and numeric state are still useful when every optional string is
  -- hostile to JSON escaping. This fixed fallback is safely below the limit
  -- for the protocol maximum of 200 rows.
  message.message = ""
  message.buffer.path = utf8_prefix(message.buffer.path, 128)
  message.buffer.version = nil
  if message.cmdline then
    message.cmdline.text = ""
    message.cmdline.cursor = 0
  end
  for index in ipairs(message.lines) do
    message.lines[index] = ""
  end
  encoded, encode_error = json_encode(message)
  if not encoded then
    return nil, encode_error
  end
  if #encoded > MAX_LINE_BYTES then
    return nil, "minimal screen snapshot exceeds the JSONL limit"
  end
  return encoded, nil
end

local function publish_screen()
  if not vim.api.nvim_get_current_win() then
    return
  end
  local window = vim.api.nvim_get_current_win()
  local buffer = vim.api.nvim_win_get_buf(window)
  local cursor = vim.api.nvim_win_get_cursor(window)
  local view = vim.fn.winsaveview()
  local top_line = math.max(1, tonumber(view.topline) or 1)
  local left_column = math.max(0, tonumber(view.leftcol) or 0)
  local content_rows = math.max(1, dimensions.rows - 1)
  local line_budget = math.max(8, math.floor(MAX_SCREEN_TEXT_BYTES / dimensions.rows))
  local source_lines = vim.api.nvim_buf_get_lines(
    buffer,
    top_line - 1,
    top_line - 1 + content_rows,
    false
  )
  local lines = {}
  for _, line in ipairs(source_lines) do
    table.insert(
      lines,
      display_slice(line, left_column, dimensions.columns, line_budget)
    )
  end
  while #lines < content_rows do
    table.insert(lines, "")
  end

  local state = current_buffer_state(buffer)
  local cmdline_type = vim.fn.getcmdtype()
  local cmdline = nil
  if cmdline_type ~= "" then
    cmdline = {
      type = cmdline_type,
      text = utf8_prefix(vim.fn.getcmdline(), MAX_LINE_BYTES / 4),
      cursor = math.max(0, vim.fn.getcmdpos() - 1),
    }
  end
  local last_message = vim.v.errmsg ~= "" and vim.v.errmsg or vim.v.statusmsg
  last_message = utf8_prefix(last_message or "", MAX_LINE_BYTES / 4)
  local status = string.format(
    " %s  %s%s",
    mode_name(vim.fn.mode(1)):upper(),
    state.path ~= "" and state.path or "[No Name]",
    state.modified and " [+]" or ""
  )
  if cmdline then
    table.insert(
      lines,
      utf8_prefix(cmdline.type .. cmdline.text, line_budget)
    )
  else
    table.insert(lines, utf8_prefix(status, line_budget))
  end

  local cursor_row = cursor[1] - top_line
  local prefix = vim.api.nvim_get_current_line():sub(1, cursor[2])
  local cursor_column = vim.fn.strdisplaywidth(prefix) - left_column
  if cmdline then
    cursor_row = dimensions.rows - 1
    cursor_column = vim.fn.strdisplaywidth(
      cmdline.type .. cmdline.text:sub(1, cmdline.cursor)
    )
  end
  local cursor_visible = presentation_focused
    and cursor_row >= 0
    and cursor_column >= 0
    and cursor_column < dimensions.columns
    and (
      (cmdline ~= nil and cursor_row == dimensions.rows - 1)
      or (cmdline == nil and cursor_row < content_rows)
    )

  screen_revision = screen_revision + 1
  local message = {
    v = VERSION,
    type = "screen",
    revision = screen_revision,
    columns = dimensions.columns,
    rows = dimensions.rows,
    lines = lines,
    cursor = {
      row = math.max(0, cursor_row),
      column = math.max(0, cursor_column),
      visible = cursor_visible,
    },
    mode = mode_name(vim.fn.mode(1)),
    buffer = state,
    cmdline = cmdline,
    message = last_message,
  }
  local encoded, encode_error = encode_bounded_screen(message, line_budget)
  if not encoded then
    protocol_error("screen-encode-failed", encode_error)
    return
  end
  local temporary_path = screen_path .. ".tmp"
  local ok, write_error = write_file(temporary_path, encoded)
  if not ok or not os.rename(temporary_path, screen_path) then
    protocol_error("screen-write-failed", tostring(write_error or "rename failed"))
  end
end

local function mark_screen_dirty()
  dirty_screen = true
end

local function content_lines(content)
  local _, line_feed_count = content:gsub("\n", "")
  local _, crlf_count = content:gsub("\r\n", "")
  local fileformat = line_feed_count > 0
      and line_feed_count == crlf_count
      and "dos"
    or "unix"
  local normalized = fileformat == "dos" and content:gsub("\r\n", "\n") or content
  local has_final_newline = normalized:sub(-1) == "\n"
  local lines = vim.split(normalized, "\n", { plain = true })
  if has_final_newline and #lines > 1 then
    table.remove(lines)
  end
  if #lines == 0 then
    lines = { "" }
  end
  return lines, has_final_newline, fileformat
end

local function install_remote_write_handler(buffer)
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    buffer = buffer,
    callback = function()
      M.request_save(buffer)
    end,
  })
  vim.api.nvim_create_autocmd("BufDelete", {
    buffer = buffer,
    once = true,
    callback = function()
      remote_buffers[buffer] = nil
    end,
  })
end

local function exec_buffer_autocmd(buffer, event, modeline)
  vim.api.nvim_buf_call(buffer, function()
    vim.api.nvim_exec_autocmds(event, {
      buffer = buffer,
      modeline = modeline,
    })
  end)
end

local function open_content(message, content)
  local id = request_id(message)
  local path = bounded_string(message.path, "path", MAX_PATH_BYTES, false)
  bounded_string(content, "content", MAX_FILE_BYTES, true)
  local version = message.version
  if version ~= nil then
    version = bounded_string(version, "version", 256, true)
  end
  if message.readonly ~= nil and type(message.readonly) ~= "boolean" then
    error("readonly must be a boolean")
  end

  local buffer = nil
  for candidate, metadata in pairs(remote_buffers) do
    if metadata.path == path and vim.api.nvim_buf_is_valid(candidate) then
      buffer = candidate
      break
    end
  end
  if buffer and vim.bo[buffer].modified then
    protocol_error("buffer-modified", "refusing to replace a modified buffer", id)
    return
  end
  if not buffer then
    buffer = vim.api.nvim_create_buf(true, false)
    vim.api.nvim_buf_set_name(buffer, path)
    vim.bo[buffer].buftype = "acwrite"
    vim.bo[buffer].swapfile = false
    install_remote_write_handler(buffer)
  end

  vim.bo[buffer].modifiable = true
  vim.bo[buffer].readonly = message.readonly == true
  exec_buffer_autocmd(buffer, "BufReadPre", false)

  local lines, has_final_newline, fileformat = content_lines(content)
  vim.api.nvim_buf_set_lines(buffer, 0, -1, false, lines)
  vim.bo[buffer].fileformat = fileformat
  vim.bo[buffer].endofline = has_final_newline
  vim.bo[buffer].fixendofline = has_final_newline
  vim.bo[buffer].modified = false
  remote_buffers[buffer] = {
    path = path,
    version = version,
    readonly = message.readonly == true,
  }
  exec_buffer_autocmd(buffer, "BufReadPost", true)
  if not vim.api.nvim_buf_is_valid(buffer) then
    error("BufReadPost removed the remote buffer")
  end
  vim.api.nvim_set_current_buf(buffer)

  local requested_line = message.line == nil and 1 or tonumber(message.line)
  local requested_column = message.column == nil and 1 or tonumber(message.column)
  if not requested_line or requested_line < 1 or requested_line % 1 ~= 0 then
    error("line must be a positive one-based integer")
  end
  if not requested_column or requested_column < 1 or requested_column % 1 ~= 0 then
    error("column must be a positive one-based integer")
  end
  local line_count = vim.api.nvim_buf_line_count(buffer)
  requested_line = math.max(1, math.min(requested_line, line_count))
  local requested_text =
    vim.api.nvim_buf_get_lines(buffer, requested_line - 1, requested_line, false)[1]
      or ""
  requested_column = math.max(
    0,
    math.min(requested_column - 1, #requested_text)
  )
  vim.api.nvim_win_set_cursor(0, { requested_line, requested_column })
  mark_screen_dirty()
  emit_event({
    type = "open-result",
    request_id = id,
    ok = true,
    path = path,
    version = version,
  })
end

local function begin_open_transfer(message)
  local id = request_id(message)
  if open_transfers[id] then
    error("open transfer already exists")
  end
  if table_size(open_transfers) >= MAX_TRANSFERS then
    error("at most four remote opens may be assembled")
  end
  local size = tonumber(message.size)
  if not size or size < 0 or size > MAX_FILE_BYTES or size % 1 ~= 0 then
    error("open transfer size is invalid")
  end
  bounded_string(message.path, "path", MAX_PATH_BYTES, false)
  open_transfers[id] = {
    message = message,
    size = size,
    offset = 0,
    chunks = {},
  }
end

local function append_open_chunk(message)
  local id = request_id(message)
  local transfer = open_transfers[id]
  if not transfer then
    error("unknown open transfer")
  end
  local offset = tonumber(message.offset)
  if offset ~= transfer.offset then
    open_transfers[id] = nil
    error("open chunk offset is not sequential")
  end
  local valid_content, content = pcall(
    bounded_string,
    message.content,
    "open chunk content",
    MAX_LINE_BYTES,
    true
  )
  if not valid_content then
    open_transfers[id] = nil
    error(content)
  end
  if transfer.offset + #content > transfer.size then
    open_transfers[id] = nil
    error("open transfer exceeds its declared size")
  end
  table.insert(transfer.chunks, content)
  transfer.offset = transfer.offset + #content
end

local function finish_open_transfer(message)
  local id = request_id(message)
  local transfer = open_transfers[id]
  if not transfer then
    error("unknown open transfer")
  end
  open_transfers[id] = nil
  if transfer.offset ~= transfer.size then
    error(
      string.format(
        "open transfer ended at %d bytes; expected %d",
        transfer.offset,
        transfer.size
      )
    )
  end
  open_content(transfer.message, table.concat(transfer.chunks))
end

local function buffer_content(buffer)
  local lines = vim.api.nvim_buf_get_lines(buffer, 0, -1, false)
  local line_ending = vim.bo[buffer].fileformat == "dos" and "\r\n" or "\n"
  local content = table.concat(lines, line_ending)
  if vim.bo[buffer].endofline then
    content = content .. line_ending
  end
  return content
end

local function next_save_request_id()
  save_sequence = save_sequence + 1
  return string.format("save-%d", save_sequence)
end

function M.request_save(buffer)
  buffer = buffer or vim.api.nvim_get_current_buf()
  local metadata = remote_buffers[buffer]
  if not metadata then
    protocol_error("not-remote-buffer", "current buffer is not backed by a remote file")
    return
  end
  if metadata.readonly then
    protocol_error("readonly", "remote buffer is read-only")
    return
  end
  for _, pending in pairs(pending_saves) do
    if pending.buffer == buffer then
      protocol_error("save-pending", "a save is already pending for this buffer")
      return
    end
  end
  if table_size(pending_saves) >= MAX_TRANSFERS then
    protocol_error(
      "save-limit",
      "at most four remote saves may be pending"
    )
    return
  end

  local write_pre_ok, write_pre_error = pcall(
    exec_buffer_autocmd,
    buffer,
    "BufWritePre",
    false
  )
  if not write_pre_ok then
    protocol_error(
      "write-pre-failed",
      tostring(write_pre_error),
      nil
    )
    error(write_pre_error)
  end

  local content = buffer_content(buffer)
  if content:find("\0", 1, true) then
    protocol_error(
      "unsupported-content",
      "remote buffers cannot be saved with NUL bytes"
    )
    return
  end
  if #content > MAX_FILE_BYTES then
    protocol_error("file-too-large", "remote buffer exceeds the 8 MiB save limit")
    return
  end
  local id = next_save_request_id()
  pending_saves[id] = {
    buffer = buffer,
    changedtick = vim.api.nvim_buf_get_changedtick(buffer),
  }

  local inline = {
    v = VERSION,
    type = "save-request",
    request_id = id,
    path = metadata.path,
    base_version = metadata.version,
    content = content,
  }
  local encoded = json_encode(inline)
  if #content <= MAX_INLINE_BYTES and encoded and #encoded <= MAX_LINE_BYTES then
    if not emit_event(inline) then
      pending_saves[id] = nil
    end
    return
  end

  local start = {
    type = "save-request-start",
    request_id = id,
    path = metadata.path,
    base_version = metadata.version,
    size = #content,
  }
  if not emit_event(start) then
    pending_saves[id] = nil
    return
  end
  save_streams[id] = {
    request_id = id,
    content = content,
    offset = 0,
  }
end

local function pump_save_stream()
  local id, stream = next(save_streams)
  if not id then
    return
  end
  if stream.offset < #stream.content then
    local chunk = utf8_chunk(stream.content, stream.offset, SAVE_CHUNK_BYTES)
    if emit_event({
      type = "save-request-chunk",
      request_id = stream.request_id,
      offset = stream.offset,
      content = chunk,
    }) then
      stream.offset = stream.offset + #chunk
    end
    return
  end
  if emit_event({
    type = "save-request-end",
    request_id = stream.request_id,
  }) then
    save_streams[id] = nil
  end
end

local function handle_save_result(message)
  local id = request_id(message)
  local pending = pending_saves[id]
  if not pending then
    error("unknown save request")
  end
  if type(message.ok) ~= "boolean" then
    error("save result ok must be a boolean")
  end
  pending_saves[id] = nil
  save_streams[id] = nil
  if not vim.api.nvim_buf_is_valid(pending.buffer) then
    return
  end
  local metadata = remote_buffers[pending.buffer]
  if message.ok then
    if metadata and message.version ~= nil then
      metadata.version = bounded_string(message.version, "version", 256, true)
    end
    if vim.api.nvim_buf_get_changedtick(pending.buffer) == pending.changedtick then
      vim.bo[pending.buffer].modified = false
    end
    local write_post_ok, write_post_error = pcall(
      exec_buffer_autocmd,
      pending.buffer,
      "BufWritePost",
      false
    )
    if not write_post_ok then
      protocol_error("write-post-failed", tostring(write_post_error), id)
    end
    emit_event({
      type = "status",
      code = "save-complete",
      request_id = id,
      path = metadata and metadata.path or "",
    })
  else
    local save_error = bounded_string(
      message.error or "remote save failed",
      "save error",
      1024,
      false
    )
    vim.notify(save_error, vim.log.levels.ERROR, { title = "Brood save" })
    protocol_error("save-failed", save_error, id)
  end
  mark_screen_dirty()
end

local function handle_resize(message)
  local columns = tonumber(message.columns)
  local rows = tonumber(message.rows)
  if not columns or columns % 1 ~= 0 or columns < 20 or columns > 400 then
    error("columns must be an integer between 20 and 400")
  end
  if not rows or rows % 1 ~= 0 or rows < 4 or rows > 200 then
    error("rows must be an integer between 4 and 200")
  end
  dimensions.columns = columns
  dimensions.rows = rows
  pcall(function()
    vim.o.columns = columns
    vim.o.lines = rows
  end)
  mark_screen_dirty()
end

local function handle_message(message)
  if type(message) ~= "table" or message.v ~= VERSION then
    error("unsupported runner protocol message")
  end
  if message.type == "key" then
    local key = bounded_string(message.key, "key", 256, false)
    -- nvim_input expands <key> notation itself. Pre-expanding it turns
    -- multi-byte special keys such as <BS> into internal K_SPECIAL bytes,
    -- which nvim_input then inserts literally.
    local accepted = vim.api.nvim_input(key)
    if accepted < #key then
      error("Neovim input queue accepted only part of the key")
    end
    mark_screen_dirty()
  elseif message.type == "text" then
    local text = bounded_string(message.text, "input text", MAX_LINE_BYTES, true)
    local accepted = vim.api.nvim_input(text)
    if accepted < #text then
      error("Neovim input queue accepted only part of the text")
    end
    mark_screen_dirty()
  elseif message.type == "input" then
    local text = bounded_string(message.text, "input text", MAX_FILE_BYTES, true)
    vim.api.nvim_paste(text, false, -1)
    mark_screen_dirty()
  elseif message.type == "paste" then
    local text = bounded_string(message.text, "paste text", MAX_LINE_BYTES, true)
    local phase = tonumber(message.phase)
    if phase ~= -1 and phase ~= 1 and phase ~= 2 and phase ~= 3 then
      error("paste phase must be -1, 1, 2, or 3")
    end
    vim.api.nvim_paste(text, false, phase)
    mark_screen_dirty()
  elseif message.type == "resize" then
    handle_resize(message)
  elseif message.type == "focus" then
    if type(message.focused) ~= "boolean" then
      error("focused must be a boolean")
    end
    presentation_focused = message.focused
    mark_screen_dirty()
  elseif message.type == "open-content" then
    open_content(
      message,
      bounded_string(message.content, "content", MAX_INLINE_BYTES, true)
    )
  elseif message.type == "open-content-start" then
    begin_open_transfer(message)
  elseif message.type == "open-content-chunk" then
    append_open_chunk(message)
  elseif message.type == "open-content-end" then
    finish_open_transfer(message)
  elseif message.type == "save-result" then
    handle_save_result(message)
  elseif message.type == "shutdown" then
    if timer then
      timer:stop()
      timer:close()
      timer = nil
    end
    local marker_ok, marker_error = write_file(shutdown_ready_path, "ready\n")
    if not marker_ok then
      error("could not publish shutdown readiness: " .. tostring(marker_error))
    end
    vim.cmd("qa!")
  else
    error("unknown runner message type: " .. tostring(message.type))
  end
end

local function process_inbox()
  local files = sorted_files(inbox_directory)
  for _, filename in ipairs(files) do
    local path = inbox_directory .. "/" .. filename
    local encoded, read_error = read_file(path)
    os.remove(path)
    if not encoded then
      protocol_error("input-read-failed", tostring(read_error))
    elseif #encoded > MAX_LINE_BYTES + 1 then
      protocol_error("input-too-large", filename .. " exceeds the JSONL limit")
    else
      local decoded, message = pcall(vim.json.decode, encoded)
      if not decoded then
        protocol_error("invalid-input", tostring(message))
      else
        local handled, handle_error = pcall(handle_message, message)
        if not handled then
          protocol_error(
            "invalid-input",
            tostring(handle_error),
            type(message) == "table" and message.request_id or nil
          )
        end
      end
    end
  end
end

function M.install()
  if installed then
    return
  end
  installed = true
  -- Neovim's default autoread plugin installs uv_fs_event watchers for normal
  -- file buffers. The Emscripten runtime has no native fs-event handle, and
  -- remote workspace changes arrive through the Brood file actor instead.
  -- This runs from --cmd, before runtime plugins and the user's init.lua.
  vim.g.loaded_autoread = 1
  vim.brood = vim.brood or {}
  vim.brood.send = function(message)
    if type(message) ~= "table" then
      error("vim.brood.send expects a table")
    end
    if message.type == nil and message.action ~= nil then
      message.type = "action"
    end
    if message.type == nil then
      message.type = "actor-message"
    end
    return emit_event(message, false)
  end

  vim.api.nvim_create_user_command("BroodDiff", function()
    emit_event({ type = "action", action = "diff" }, false)
  end, { force = true, desc = "Open a backend Git diff terminal" })

  vim.api.nvim_create_user_command("BroodTerminal", function(options)
    emit_event({
      type = "action",
      action = "terminal",
      command = bounded_string(options.args, "terminal command", 8 * 1024, true),
    }, false)
  end, {
    force = true,
    nargs = "*",
    desc = "Open a Brood terminal, optionally running a command",
  })

  vim.api.nvim_create_user_command("BroodAgent", function(options)
    emit_event({
      type = "action",
      action = "agent",
      prompt = bounded_string(options.args, "agent prompt", 8 * 1024, true),
    }, false)
  end, {
    force = true,
    nargs = "*",
    desc = "Send work to the Brood coding agent",
  })

  vim.api.nvim_create_user_command("BroodMessage", function(options)
    local target, body_source = options.args:match("^(%S+)%s+(.+)$")
    target = bounded_string(target, "actor target", 128, false)
    body_source = bounded_string(body_source, "actor message", 8 * 1024, false)
    local decoded, body = pcall(vim.json.decode, body_source)
    if not decoded then
      body = body_source
    end
    emit_event({
      type = "actor-message",
      target = target,
      body = body,
    }, false)
  end, {
    force = true,
    nargs = "+",
    desc = "Send JSON or text to a Brood actor",
  })

  vim.keymap.set("n", "<Plug>(BroodDiff)", "<Cmd>BroodDiff<CR>")
  vim.keymap.set("n", "<Plug>(BroodTerminal)", ":<C-U>BroodTerminal ")
  vim.keymap.set("n", "<Plug>(BroodAgent)", ":<C-U>BroodAgent ")
  vim.keymap.set("n", "<Plug>(BroodMessage)", ":<C-U>BroodMessage ")
end

local function read_startup_json(path, fallback)
  local source = read_file(path)
  if not source then
    return fallback
  end
  local ok, decoded = pcall(vim.json.decode, source)
  if ok and type(decoded) == "table" then
    return decoded
  end
  return fallback
end

function M.start()
  if started then
    return
  end
  started = true
  M.install()
  dimensions = read_startup_json("/brood/io/dimensions.json", dimensions)

  local group = vim.api.nvim_create_augroup("BroodWasmBridge", { clear = true })
  vim.api.nvim_create_autocmd({
    "BufEnter",
    "CmdlineChanged",
    "CmdlineEnter",
    "CmdlineLeave",
    "CursorMoved",
    "CursorMovedI",
    "ModeChanged",
    "TextChanged",
    "TextChangedI",
    "VimResized",
  }, {
    group = group,
    callback = mark_screen_dirty,
  })

  timer = vim.uv.new_timer()
  timer:start(0, 16, vim.schedule_wrap(function()
    flush_critical_events()
    process_inbox()
    pump_save_stream()
    flush_critical_events()
    if dirty_screen then
      dirty_screen = false
      publish_screen()
    end
  end))

  emit_event({
    type = "ready",
    runtime = "neovim-wasm-headless",
    config_marker = vim.g.brood_config_marker,
    mirror = read_startup_json("/brood/io/mirror.json", {}),
    capabilities = {
      pure_lua_plugins = true,
      native_modules = false,
      subprocesses = false,
      remote_files = true,
      chunked_files = true,
      headless_viewport = true,
      composed_grid = false,
      command_line = true,
      messages = true,
      popupmenu = false,
      split_windows = false,
      max_file_bytes = MAX_FILE_BYTES,
    },
  })
  mark_screen_dirty()
end

return M

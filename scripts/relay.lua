-- The relay: several streams carried over one ssh connection.
--
-- nvmux reaches a host's sessions through one ssh ControlMaster, adding a
-- forward to it per session. A client with no ControlMaster to add forwards
-- to -- ssh on Windows has none -- needs another way to open a shell and a
-- connection to every session's socket without a new ssh connection for each,
-- which would cost a handshake apiece and, for some keys, a passphrase or a
-- touch every time. So it opens one connection, and this runs at the far end
-- of it, under the Neovim every session host already has: it splits this
-- process's stdin and stdout into channels, each one a `sh -s` for scripts or
-- a connection to a unix socket, and carries each channel's bytes in frames.
--
-- Started by scripts/boot.sh as `nvim --clean --headless -l <this> <secret>`;
-- src/mux.rs is the other end, and says what the protocol is for. The frame:
--
--   kind (1 byte) | channel (4 bytes, big-endian) | length (4) | payload
--
-- Plain Lua and `vim.uv`, and no `bit` library: this is the one place nvmux
-- runs code of its own inside somebody's Neovim, so it asks as little of that
-- Neovim as it can. `vim.loop` is the older name of the same thing.

local uv = vim.uv or vim.loop

local secret = arg[1]
if not secret or secret == '' then
  io.stderr:write('usage: nvim --clean --headless -l relay.lua <secret>\n')
  os.exit(64)
end

-- Every `nvim` starts a server of its own, `-l` included, and a relay has no
-- use for one: left up it is a socket in the user's runtime directory that
-- something could find and attach to.
pcall(vim.fn.serverstop, vim.v.servername)

-- Must match src/mux.rs.
local VERSION = 1
local OPEN_SOCKET, OPEN_SHELL, OPENED, REFUSED = 1, 2, 3, 4
local DATA, STDERR, EOF, CLOSE, EXIT, CREDIT, KILL = 5, 6, 7, 8, 9, 10, 11

-- How many bytes of one channel may be on their way to the other end without
-- having been taken yet: past it, the sender waits for a CREDIT. It is what
-- keeps one busy channel -- a session repainting while its client is not read
-- -- from queueing without bound on either side, and from holding up every
-- other channel behind it. The same number on both sides.
local WINDOW = 256 * 1024

local stdin = uv.new_pipe(false)
local stdout = uv.new_pipe(false)
if not stdin:open(0) or not stdout:open(1) then
  io.stderr:write('nvmux relay: stdin and stdout must be pipes\n')
  os.exit(66)
end

local function u32(n)
  return string.char(
    math.floor(n / 16777216) % 256,
    math.floor(n / 65536) % 256,
    math.floor(n / 256) % 256,
    n % 256
  )
end

local function u32_at(s, i)
  local a, b, c, d = s:byte(i, i + 3)
  return ((a * 256 + b) * 256 + c) * 256 + d
end

local function send(kind, id, payload)
  payload = payload or ''
  stdout:write(string.char(kind) .. u32(id) .. u32(#payload) .. payload)
end

-- An error in a callback would otherwise be printed and forgotten, leaving a
-- relay that carries on with a channel half-handled. Better to go, and say why:
-- the ssh connection closing is something the other end handles, and whatever
-- is written here reaches it as ssh's stderr.
local finished = false

local function fail(why)
  io.stderr:write('nvmux relay: ', tostring(why), '\n')
  os.exit(70)
end

local function guard(fn)
  return function(...)
    local ok, err = pcall(fn, ...)
    if not ok then
      fail(err)
    end
  end
end

-- id -> channel. A socket channel holds its pipe; a shell channel its process
-- and three pipes.
local chans = {}

local function close_handle(h)
  if h and not h:is_closing() then
    h:close()
  end
end

-- What this channel sends is paused while its window is spent, by no longer
-- reading what feeds it; a CREDIT reads again. A socket has one such stream,
-- a shell two.
local function readers(c)
  if c.shell then
    return { { c.out, DATA }, { c.err, STDERR } }
  end
  return { { c.pipe, DATA } }
end

local start_reading

local function pause(c)
  c.paused = true
  for _, r in ipairs(readers(c)) do
    if r[1] and not r[1]:is_closing() then
      r[1]:read_stop()
    end
  end
end

local function resume(id, c)
  c.paused = false
  for _, r in ipairs(readers(c)) do
    if r[1] and not r[1]:is_closing() and not c.ended[r[2]] then
      start_reading(id, c, r[1], r[2])
    end
  end
end

-- A shell channel is over once both its streams have ended and its process has
-- exited, in whatever order the three arrive: only then has everything it said
-- been sent, and the exit is the last word.
local function shell_done(id, c)
  if not (c.ended[DATA] and c.ended[STDERR] and c.status) then
    return
  end
  send(EXIT, id, c.status)
  send(CLOSE, id)
  chans[id] = nil
  close_handle(c.stdin)
  close_handle(c.out)
  close_handle(c.err)
  close_handle(c.proc)
end

local function forget(id, c, tell)
  if chans[id] ~= c then
    return
  end
  chans[id] = nil
  if tell then
    send(CLOSE, id)
  end
  if c.shell then
    if not c.status and c.proc and not c.proc:is_closing() then
      c.proc:kill('sigkill')
    end
    close_handle(c.stdin)
    close_handle(c.out)
    close_handle(c.err)
    close_handle(c.proc)
  else
    close_handle(c.pipe)
  end
end

start_reading = function(id, c, h, kind)
  h:read_start(guard(function(err, data)
    if chans[id] ~= c then
      return
    end
    if err or not data then
      h:read_stop()
      c.ended[kind] = true
      if c.shell then
        send(EOF, id, kind == DATA and 'out' or 'err')
        shell_done(id, c)
      else
        -- The socket's far end has gone: the session hung up, or ended.
        forget(id, c, true)
      end
      return
    end
    send(kind, id, data)
    c.window = c.window - #data
    if c.window <= 0 and not c.paused then
      pause(c)
    end
  end))
end

local function open_socket(id, path)
  local pipe = uv.new_pipe(false)
  local c = { pipe = pipe, window = WINDOW, credit = 0, ended = {} }
  chans[id] = c
  pipe:connect(path, guard(function(err)
    if chans[id] ~= c then
      close_handle(pipe)
      return
    end
    if err then
      chans[id] = nil
      close_handle(pipe)
      send(REFUSED, id, err)
      return
    end
    send(OPENED, id)
    start_reading(id, c, pipe, DATA)
  end))
end

local function open_shell(id)
  local c = {
    shell = true,
    stdin = uv.new_pipe(false),
    out = uv.new_pipe(false),
    err = uv.new_pipe(false),
    window = WINDOW,
    credit = 0,
    ended = {},
  }
  local proc, pid = uv.spawn('sh', {
    args = { '-s' },
    stdio = { c.stdin, c.out, c.err },
  }, guard(function(code, signal)
    if signal and signal ~= 0 then
      c.status = '-1 ' .. signal
    else
      c.status = tostring(code) .. ' 0'
    end
    if chans[id] == c then
      shell_done(id, c)
    else
      close_handle(c.proc)
    end
  end))
  if not proc then
    close_handle(c.stdin)
    close_handle(c.out)
    close_handle(c.err)
    send(REFUSED, id, tostring(pid))
    return
  end
  c.proc = proc
  chans[id] = c
  send(OPENED, id, tostring(pid))
  start_reading(id, c, c.out, DATA)
  start_reading(id, c, c.err, STDERR)
end

-- Bytes for a channel, from the other end. Credited back once written, which
-- is what bounds how much can wait here for a socket or a shell that is slow
-- to take it.
local function write_to(id, c, data)
  local sink = c.shell and c.stdin or c.pipe
  if not sink or sink:is_closing() then
    return
  end
  sink:write(data, guard(function()
    if chans[id] ~= c then
      return
    end
    c.credit = c.credit + #data
    if c.credit >= WINDOW / 2 then
      send(CREDIT, id, u32(c.credit))
      c.credit = 0
    end
  end))
end

local function dispatch(kind, id, payload)
  local c = chans[id]
  if kind == OPEN_SOCKET then
    open_socket(id, payload)
  elseif kind == OPEN_SHELL then
    open_shell(id)
  elseif not c then
    -- A channel this end has already let go of: nothing to do.
    return
  elseif kind == DATA then
    write_to(id, c, payload)
  elseif kind == EOF then
    local sink = c.shell and c.stdin or c.pipe
    if sink and not sink:is_closing() then
      sink:shutdown()
    end
  elseif kind == CREDIT then
    c.window = c.window + u32_at(payload, 1)
    if c.paused and c.window > 0 then
      resume(id, c)
    end
  elseif kind == KILL then
    if c.shell and not c.status and c.proc and not c.proc:is_closing() then
      c.proc:kill(payload ~= '' and payload or 'sigkill')
    end
  elseif kind == CLOSE then
    forget(id, c, false)
  end
end

-- Frames arrive in whatever pieces the pipe hands over, so a frame is only
-- taken once all of it is here.
local pending = ''

local function on_input(err, data)
  if err or not data then
    finished = true
    return
  end
  pending = pending .. data
  local at, len = 1, #pending
  while len - at + 1 >= 9 do
    local n = u32_at(pending, at + 5)
    if len - at + 1 < 9 + n then
      break
    end
    local kind, id = pending:byte(at), u32_at(pending, at + 1)
    local payload = pending:sub(at + 9, at + 8 + n)
    at = at + 9 + n
    dispatch(kind, id, payload)
  end
  pending = pending:sub(at)
end

-- The line the other end waits for before it sends a frame: everything before
-- it on this stream was the login shell's, and everything after it is frames.
stdout:write('NVMUX_RELAY_' .. secret .. ' ' .. VERSION .. '\n')
stdin:read_start(guard(on_input))

while not finished do
  vim.wait(60000, function()
    return finished
  end, 1000)
end

-- The other end has gone, and with it anyone to read what a channel has to
-- say. Sessions are not channels -- each is its own detached process -- so
-- they go on; what goes with the relay is its shells and its connections.
for id, c in pairs(chans) do
  forget(id, c, false)
end

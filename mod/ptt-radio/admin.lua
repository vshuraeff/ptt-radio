local channels = require("channels")
local admin = {}
local PREFIX = "ptt-radio-admin-"
local LOCALE = "ptt-radio." .. PREFIX
local ROOT = PREFIX .. "section"
local changed

function admin.configure(callback)
  changed = callback
end

local function message(actor, key)
  local text = { LOCALE .. key }
  if actor == 0 then game.print(text) else game.get_player(actor).print(text) end
end

-- quoted names use double quotes; indices also address names containing a quote.
local function tokens(text)
  local out, at = {}, 1
  while at <= #text do
    local char = text:sub(at, at)
    if char:match("%s") then
      at = at + 1
    elseif char == '"' then
      local last = text:find('"', at + 1, true)
      if not last or (last < #text and not text:sub(last + 1, last + 1):match("%s")) then return nil end
      out[#out + 1], at = text:sub(at + 1, last - 1), last + 1
    else
      local last = text:find("%s", at) or #text + 1
      local token = text:sub(at, last - 1)
      if token:find('"', 1, true) then return nil end
      out[#out + 1], at = token, last
    end
  end
  return out
end

local function player_index(text)
  if not text then return nil end
  local index = tonumber(text)
  if index and (index < 1 or index % 1 ~= 0 or index > 65535) then return nil end
  local player = game.get_player(index or text)
  return player and player.index or nil
end

local function channel_key(text)
  if text == "ANY" or text == channels.ANY then return channels.ANY end
  if text == "HERE" or text == channels.HERE then return channels.HERE end
  if not text then return nil end
  local kind, name = text:match("^([^/]+)/(.+)$")
  return channels.key({ type = kind or "item", name = name or text })
end

-- command and gui inputs meet here; all state changes stay in channels.policy_*.
local function execute(actor, args)
  local player = actor ~= 0 and game.get_player(actor) or nil
  if actor ~= 0 and not (player and player.admin) then return false, "not-admin" end
  if not args then return false, "usage" end
  local op, n = args[1], #args
  if op == "channel" and n == 2 then
    local key = args[2] ~= "none" and channel_key(args[2]) or nil
    if args[2] ~= "none" and not key then return false, "invalid-channel" end
    return channels.policy_admin_channel(actor, key)
  elseif (op == "ban" or op == "unban" or op == "pin" or op == "unpin") and n == 2 then
    local key = channel_key(args[2])
    if not key then return false, "invalid-channel" end
    if op == "ban" or op == "unban" then return channels.policy_ban(actor, key, op == "ban") end
    return channels.policy_pin(actor, key, op == "pin")
  elseif op == "publisher" and n == 3 and (args[2] == "add" or args[2] == "remove") then
    local target = player_index(args[3])
    if not target then return false, "invalid-player" end
    return channels.policy_publisher(actor, target, args[2] == "add")
  elseif (op == "mute" or op == "unmute" or op == "unlock" or op == "release") and n == 2 then
    local target = player_index(args[2])
    if not target then return false, "invalid-player" end
    if op == "mute" or op == "unmute" then return channels.policy_mute(actor, target, op == "mute") end
    if op == "unlock" then return channels.policy_lock(actor, target, nil) end
    return channels.policy_confine(actor, target, nil)
  elseif op == "lock" and n == 3 then
    local target = player_index(args[2])
    if not target then return false, "invalid-player" end
    return channels.policy_lock(actor, target, args[3])
  elseif (op == "deny" and n == 4) or (op == "allow" and n == 3) then
    local target, key = player_index(args[2]), channel_key(args[3])
    if not target then return false, "invalid-player" end
    if not key then return false, "invalid-channel" end
    return channels.policy_deny(actor, target, key, op == "deny" and args[4] or nil)
  elseif op == "confine" and n >= 3 then
    local target, keys = player_index(args[2]), {}
    if not target then return false, "invalid-player" end
    for i = 3, n do
      local key = channel_key(args[i])
      if not key then return false, "invalid-channel" end
      keys[#keys + 1] = key
    end
    return channels.policy_confine(actor, target, keys)
  end
  return false, "usage"
end

local function apply(actor, args)
  local did_change, reason = execute(actor, args)
  if did_change then
    if changed then changed() end
    message(actor, "applied")
    if args[1] == "channel" and channel_key(args[2]) == channels.ANY then message(actor, "any-warning") end
  else
    message(actor, reason or "unchanged")
  end
end

function admin.register()
  commands.add_command("radio-admin", { LOCALE .. "command-help" }, function(event)
    apply(event.player_index or 0, tokens(event.parameter or ""))
  end)
end

local function summary()
  local policy = storage.radio_policy
  local lines = { { LOCALE .. "current-channel", policy.admin_channel or "none" } }
  local function list(set)
    local keys = {}
    for key in pairs(set) do keys[#keys + 1] = key end
    table.sort(keys)
    return table.concat(keys, ", ")
  end
  lines[#lines + 1] = { LOCALE .. "current-global", list(policy.banned), list(policy.pinned) }
  for _, player in pairs(game.players) do
    local p = player.index
    local denials = {}
    for key, mode in pairs(policy.deny[p] or {}) do denials[#denials + 1] = key .. ":" .. mode end
    table.sort(denials)
    if policy.muted[p] or policy.publishers[p] or policy.lock[p] or policy.confine[p] or #denials > 0 then
      lines[#lines + 1] = { LOCALE .. "current-player", player.name, p,
        policy.muted[p] and "yes" or "no", policy.publishers[p] and "yes" or "no",
        policy.lock[p] or "none", policy.confine[p] and list(policy.confine[p]) or "*", table.concat(denials, ", ") }
    end
  end
  return lines
end

local function label(parent, key)
  local item = parent.add({ type = "label", caption = { LOCALE .. key } })
  item.style.single_line = false
  item.style.maximal_width = 560
  return item
end

local function button(parent, action)
  parent.add({ type = "button", name = PREFIX .. action, caption = { LOCALE .. action } })
end

local function render_summary(parent)
  local current = parent[PREFIX .. "current"]
  if current then current.clear() else
    current = parent.add({ type = "flow", name = PREFIX .. "current", direction = "vertical" })
  end
  for _, caption in ipairs(summary()) do
    local line = current.add({ type = "label", caption = caption })
    line.style.single_line = false
    line.style.maximal_width = 560
  end
end

function admin.render(player, parent)
  if parent[ROOT] then parent[ROOT].destroy() end
  if not player or player.index == 0 or not player.admin then return end
  local root = parent.add({ type = "frame", name = ROOT, caption = { LOCALE .. "title" }, direction = "vertical" })
  label(root, "rules")
  label(root, "any-warning")
  label(root, "signal-help")
  local selection = root.add({ type = "flow", direction = "horizontal" })
  selection.add({ type = "drop-down", name = PREFIX .. "kind", items = { { LOCALE .. "signal" }, "ANY", "HERE" }, selected_index = 1 })
  selection.add({ type = "choose-elem-button", name = PREFIX .. "signal", elem_type = "signal" })
  local channel = root.add({ type = "flow", direction = "horizontal" })
  button(channel, "channel"); button(channel, "clear-channel")
  local global = root.add({ type = "flow", direction = "horizontal" })
  for _, op in ipairs({ "ban", "unban", "pin", "unpin" }) do button(global, op) end
  label(root, "target-help")
  root.add({ type = "textfield", name = PREFIX .. "target", text = "" })
  local target = root.add({ type = "flow", direction = "horizontal" })
  for _, op in ipairs({ "mute", "unmute", "publisher-add", "publisher-remove" }) do button(target, op) end
  local modes = root.add({ type = "flow", direction = "horizontal" })
  modes.add({ type = "drop-down", name = PREFIX .. "deny-mode", items = { "tx", "rx", "both" }, selected_index = 3 })
  button(modes, "deny"); button(modes, "allow")
  local lock = root.add({ type = "flow", direction = "horizontal" })
  lock.add({ type = "drop-down", name = PREFIX .. "lock-mode", items = { "tx", "rx", "all" }, selected_index = 3 })
  button(lock, "lock"); button(lock, "unlock")
  label(root, "confine-help")
  root.add({ type = "textfield", name = PREFIX .. "confine-keys", text = "" }).style.width = 500
  local confinement = root.add({ type = "flow", direction = "horizontal" })
  button(confinement, "confine"); button(confinement, "release")
  render_summary(root)
end

-- widgets in the nested rows are found by name without assuming the settings window's layout.
local function field(root, name)
  if root.name == PREFIX .. name then return root end
  for _, child in pairs(root.children) do
    local found = field(child, name)
    if found then return found end
  end
end

local function gui_context(event)
  local element = event.element
  if not (element and element.valid and element.name:sub(1, #PREFIX) == PREFIX) then return false end
  local root = element
  while root and root.name ~= ROOT do root = root.parent end
  local player = event.player_index and event.player_index > 0 and game.get_player(event.player_index) or nil
  if not player or not player.admin then
    if root then root.destroy() end
    if player then message(player.index, "not-admin") end
    return true
  end
  return true, player, root
end

function admin.on_gui_elem_changed(event)
  local handled, player, root = gui_context(event)
  if player and root and event.element.name == PREFIX .. "signal" then field(root, "kind").selected_index = 1 end
  return handled
end

function admin.on_gui_click(event)
  local handled, player, root = gui_context(event)
  if not player or not root then return handled end
  local op = event.element.name:sub(#PREFIX + 1)
  local target = field(root, "target").text
  local kind = field(root, "kind").selected_index
  local signal = field(root, "signal").elem_value
  local key = kind == 2 and channels.ANY or kind == 3 and channels.HERE or channels.key(signal)
  local args
  if op == "channel" or op == "ban" or op == "unban" or op == "pin" or op == "unpin" then
    args = { op, key or "" }
  elseif op == "clear-channel" then args = { "channel", "none" }
  elseif op == "publisher-add" or op == "publisher-remove" then
    args = { "publisher", op == "publisher-add" and "add" or "remove", target }
  elseif op == "mute" or op == "unmute" or op == "unlock" or op == "release" then args = { op, target }
  elseif op == "lock" then args = { op, target, ({ "tx", "rx", "all" })[field(root, "lock-mode").selected_index] }
  elseif op == "deny" then args = { op, target, key or "", ({ "tx", "rx", "both" })[field(root, "deny-mode").selected_index] }
  elseif op == "allow" then args = { op, target, key or "" }
  elseif op == "confine" then
    local keys = tokens(field(root, "confine-keys").text)
    if keys then
      args = { op, target }
      for _, value in ipairs(keys) do args[#args + 1] = value end
    end
  else return handled end
  apply(player.index, args)
  if root.valid then render_summary(root) end
  return true
end

return admin

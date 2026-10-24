-- ptt-radio settings window: the player's own radio settings, rebuilt from channels.state.
--
-- the window is a gui.screen frame opened by the shortcut or the hotkey, closed by its
-- button or by Esc (player.opened + on_gui_closed). it holds the radio on/off switch, the
-- tx picker, the rx list, presets, the known-channel list, the honest-clients note and,
-- when render_admin was injected, a flow at the bottom for the administration section.
--
-- every mutation goes through channels.*: the module only decides what to show disabled
-- and why, reading storage.radio_policy for the tooltip. the on/off switch is never
-- disabled, whatever the locks say.
--
-- every element is named "ptt-radio-settings-<action>[-<arg>]" and carries tags
-- { action, arg }, so the dispatchers below claim only their own elements. the only gui
-- state kept is the last refusal message per player, in storage.settings_gui.
--
-- globals (storage, game) are read at call time, never captured.

local channels = require("channels")

local settings_gui = {}

local PREFIX = "ptt-radio-settings-"
local WINDOW = PREFIX .. "window"
local STATUS = PREFIX .. "status"
local ADMIN = PREFIX .. "admin"
local BODY_WIDTH, MAX_HEIGHT = 400, 640
-- channels.set_presets enforces the same limit
local MAX_PRESETS = 8
local RED = { r = 1, g = 0.4, b = 0.4, a = 1 }
local GREY = { r = 0.7, g = 0.7, b = 0.7, a = 1 }
local PRESET_BUTTONS = {
  { action = "preset-up", sprite = "utility/collapse" },
  { action = "preset-down", sprite = "utility/expand" },
  { action = "preset-remove", sprite = "utility/trash" },
}

-- injected by control.lua: the audience-changed callback and the optional admin section
local on_changed, render_admin

function settings_gui.configure(changed, admin_section)
  on_changed, render_admin = changed, admin_section
end

function settings_gui.init()
  storage.settings_gui = storage.settings_gui or { message = {} }
end

local function text(key, ...)
  return { "ptt-radio-settings." .. key, ... }
end

local function name_of(key)
  if key == channels.ANY then return text("name-any") end
  if key == channels.HERE then return text("name-here") end
  return key
end

local function caption(key)
  local sprite = channels.sprite(key)
  return { "", sprite and ("[img=" .. sprite .. "] ") or "", name_of(key) }
end

-- a channel key as a SignalID for the picker; the reserved keys are not signals
local function signal(key)
  local signal_type, name = string.match(key, "^([^/]+)/(.+)$")
  if key == channels.ANY or key == channels.HERE or not signal_type then return nil end
  return { type = signal_type, name = name }
end

-- why a lock refuses scope ("tx", "rx" or "all"), as a tooltip, or nil
local function lock_text(index, scope)
  local mode = storage.radio_policy.lock[index]
  if mode == "all" or mode == scope then return text("lock-" .. mode) end
end

-- why the policy refuses key for mode ("tx" or "rx"), as a tooltip, or nil; this mirrors
-- the refusals channels.set_tx and channels.set_rx return, the admin channel exempt
local function policy_text(index, key, mode)
  local policy = storage.radio_policy
  if key == policy.admin_channel then return nil end
  local deny = policy.deny[index] and policy.deny[index][key]
  local reason = policy.banned[key] and "banned"
    or (deny == mode or deny == "both") and "denied"
    or (policy.confine[index] and not policy.confine[index][key]) and "confined"
  return reason and text("refused-" .. reason) or nil
end

local function disable(element, reason)
  element.enabled = false
  element.tooltip = reason
end

local function control(parent, spec, action, arg)
  spec.name = PREFIX .. action .. (arg ~= nil and "-" .. arg or "")
  spec.tags = { action = action, arg = arg }
  return parent.add(spec)
end

local function heading(parent, key)
  parent.add({ type = "line" })
  parent.add({ type = "label", caption = text(key), style = "bold_label" })
end

local function note(parent, caption_text, color, tooltip)
  local label = parent.add({ type = "label", caption = caption_text, tooltip = tooltip })
  label.style.single_line = false
  label.style.maximal_width = BODY_WIDTH
  if color then label.style.font_color = color end
  return label
end

local function row(parent)
  local line = parent.add({ type = "flow", direction = "horizontal" })
  line.style.vertical_align = "center"
  return line
end

local function titlebar(frame)
  local bar = frame.add({ type = "flow", direction = "horizontal" })
  bar.drag_target = frame
  bar.style.horizontal_spacing = 8
  bar.add({ type = "label", caption = text("title"), style = "frame_title", ignored_by_interaction = true })
  local filler = bar.add({ type = "empty-widget", style = "draggable_space_header", ignored_by_interaction = true })
  filler.style.horizontally_stretchable = true
  filler.style.height = 24
  control(bar, { type = "sprite-button", sprite = "utility/close", style = "frame_action_button", tooltip = text("close") }, "close")
end

local function off_section(body, s)
  -- never disabled: locks cover channel and preset selection, not the switch
  control(body, { type = "checkbox", caption = text("off-caption"), tooltip = text("off-tooltip"), state = s.off }, "off")
end

local function tx_section(body, player, s)
  local index, policy = player.index, storage.radio_policy
  heading(body, "tx-title")
  if policy.muted[index] then note(body, text("tx-muted"), RED) end
  if s.tx == policy.admin_channel and not (policy.publishers[index] or player.admin) then
    note(body, text("tx-not-publisher"), RED)
  end
  local line = row(body)
  local picker = control(line, { type = "choose-elem-button", elem_type = "signal" }, "tx-pick")
  picker.elem_value = signal(s.tx)
  local lock = lock_text(index, "tx")
  if lock then disable(picker, lock) else picker.tooltip = text("tx-pick-tooltip") end
  for _, key in ipairs({ channels.ANY, channels.HERE }) do
    local button = control(line, { type = "button", caption = caption(key), toggled = s.tx == key }, "tune", key)
    local why = lock or policy_text(index, key, "tx")
    if why then disable(button, why) end
  end
  body.add({ type = "label", caption = { "", text("tx-current"), " ", caption(s.tx) } })
end

local function rx_section(body, player, s, known)
  local index, policy = player.index, storage.radio_policy
  heading(body, "rx-title")
  local counts, keys, seen, subscribed = {}, {}, {}, {}
  local function list(key)
    if not seen[key] then
      seen[key] = true
      keys[#keys + 1] = key
    end
  end
  list(s.tx)
  for _, entry in ipairs(known) do
    counts[entry.key] = entry.count
    list(entry.key)
  end
  for _, key in ipairs(s.rx) do
    subscribed[key] = true
    list(key)
  end
  for _, key in ipairs(keys) do
    -- the model refuses unsubscribing the admin channel even while off, so it stays locked
    local mandatory = key == policy.admin_channel
    local checked = mandatory or key == s.tx or subscribed[key] == true
    local count = counts[key] and (" (" .. counts[key] .. ")") or ""
    local checkbox = control(body, { type = "checkbox", caption = { "", caption(key), count }, state = checked }, "rx", key)
    local why
    if mandatory then
      why = text("rx-admin")
    elseif key == s.tx then
      why = text("rx-tx-always")
    else
      why = lock_text(index, "rx") or (not checked and policy_text(index, key, "rx")) or nil
    end
    if why then disable(checkbox, why) end
  end
end

local function presets_section(body, player, s)
  local index = player.index
  heading(body, "presets-title")
  local locked = lock_text(index, "all")
  local present = false
  if #s.presets == 0 then note(body, text("presets-empty"), GREY) end
  for i, key in ipairs(s.presets) do
    present = present or key == s.tx
    local line = row(body)
    line.add({ type = "label", caption = caption(key) }).style.horizontally_stretchable = true
    for _, spec in ipairs(PRESET_BUTTONS) do
      local button = control(line, { type = "sprite-button", sprite = spec.sprite, style = "tool_button", tooltip = text(spec.action) }, spec.action, i)
      if locked then
        disable(button, locked)
      elseif (spec.action == "preset-up" and i == 1) or (spec.action == "preset-down" and i == #s.presets) then
        button.enabled = false
      end
    end
  end
  local line = row(body)
  local add = control(line, { type = "button", caption = text("preset-add") }, "preset-add")
  if locked then
    disable(add, locked)
  elseif #s.presets >= MAX_PRESETS then
    disable(add, text("preset-full"))
  elseif present then
    disable(add, text("preset-present"))
  else
    add.tooltip = text("preset-add-tooltip")
  end
  local cycle = control(line, { type = "button", caption = text("cycle") }, "cycle")
  local why = lock_text(index, "tx") or (#s.presets == 0 and text("refused-no-presets")) or nil
  if why then disable(cycle, why) else cycle.tooltip = text("cycle-tooltip") end
end

local function known_section(body, player, s, known)
  local index = player.index
  heading(body, "known-title")
  note(body, text("known-hint"), GREY)
  if #known == 0 then note(body, text("known-empty"), GREY) end
  for _, entry in ipairs(known) do
    local line = row(body)
    local button = control(line, {
      type = "sprite-button", sprite = channels.sprite(entry.key), number = entry.count, style = "slot_button",
      toggled = entry.key == s.tx, tooltip = text("known-tune"),
    }, "known", entry.key)
    local why = lock_text(index, "tx") or policy_text(index, entry.key, "tx")
    if why then disable(button, why) end
    line.add({ type = "label", caption = name_of(entry.key) })
    if entry.admin then
      line.add({ type = "label", caption = text("known-admin"), tooltip = text("rx-admin") }).style.font_color = RED
    end
  end
end

local function build(player, focus)
  local screen = player.gui.screen
  local old = screen[WINDOW]
  local location = old and old.location
  local was_opened = old ~= nil and player.opened == old
  if old then old.destroy() end
  local frame = screen.add({ type = "frame", name = WINDOW, direction = "vertical" })
  if location then frame.location = location else frame.auto_center = true end
  titlebar(frame)
  local pane = frame.add({ type = "scroll-pane" })
  pane.style.maximal_height = MAX_HEIGHT
  pane.style.minimal_width = BODY_WIDTH
  local body = pane.add({ type = "flow", direction = "vertical" })
  local s, known = channels.state(player.index), channels.known(player.index)
  local message = storage.settings_gui.message[player.index]
  if message then note(body, message, RED).name = STATUS end
  if storage.companions and storage.companions[player.index]
    and not (storage.pol1 and storage.pol1[player.index]) then
    note(body, text("upgrade-required"), RED)
  end
  off_section(body, s)
  tx_section(body, player, s)
  rx_section(body, player, s, known)
  presets_section(body, player, s)
  known_section(body, player, s, known)
  body.add({ type = "line" })
  note(body, text("honest-short"), GREY, text("honest-note"))
  if render_admin then
    render_admin(player, body.add({ type = "flow", name = ADMIN, direction = "vertical" }))
  end
  if focus or was_opened then player.opened = frame end
end

function settings_gui.open(player)
  storage.settings_gui.message[player.index] = nil
  build(player, true)
end

function settings_gui.close(player)
  storage.settings_gui.message[player.index] = nil
  local frame = player.gui.screen[WINDOW]
  if frame then frame.destroy() end
end

function settings_gui.toggle(player)
  if player.gui.screen[WINDOW] then settings_gui.close(player) else settings_gui.open(player) end
end

function settings_gui.refresh(player)
  if player.gui.screen[WINDOW] then build(player) end
end

-- runs after a channels mutation: a change notifies the audience and refreshes every open
-- window (the known list and its counts are shared); a refusal is kept as the message
local function finish(player, changed, reason)
  local message = not changed and reason and { "", text("refused"), " ", text("refused-" .. reason) } or nil
  storage.settings_gui.message[player.index] = message
  if changed then
    if on_changed then on_changed() end
    for _, other in pairs(game.connected_players) do settings_gui.refresh(other) end
  else
    settings_gui.refresh(player)
  end
  -- with no window the refusal would be invisible
  if message and not player.gui.screen[WINDOW] then player.print(message) end
end

local function edit_presets(player, edit)
  local keys = channels.state(player.index).presets
  edit(keys)
  return channels.set_presets(player.index, keys)
end

local function swap(keys, i, j)
  if keys[i] and keys[j] then keys[i], keys[j] = keys[j], keys[i] end
end

local function tune(player, key)
  return channels.set_tx(player.index, key)
end

local CLICK = {
  close = function(player) settings_gui.close(player) end,
  tune = tune,
  known = tune,
  ["preset-add"] = function(player)
    return edit_presets(player, function(keys) keys[#keys + 1] = channels.tx(player.index) end)
  end,
  ["preset-remove"] = function(player, i)
    return edit_presets(player, function(keys) if keys[i] then table.remove(keys, i) end end)
  end,
  ["preset-up"] = function(player, i)
    return edit_presets(player, function(keys) swap(keys, i - 1, i) end)
  end,
  ["preset-down"] = function(player, i)
    return edit_presets(player, function(keys) swap(keys, i, i + 1) end)
  end,
  cycle = function(player) return channels.cycle(player.index) end,
}

local ELEM = {
  ["tx-pick"] = function(player, _, element)
    local key = channels.key(element.elem_value)
    -- a cleared picker only restores the old value on the rebuild
    if not key then return false, element.elem_value and "unusable-signal" or nil end
    return channels.set_tx(player.index, key)
  end,
}

local CHECKED = {
  rx = function(player, key, element) return channels.set_rx(player.index, key, element.state) end,
  off = function(player, _, element) return channels.set_off(player.index, element.state) end,
}

-- the hotkey path: the same cycle as the button, with a chat line when no window shows it
function settings_gui.cycle(player)
  local changed, reason = CLICK.cycle(player)
  finish(player, changed, reason)
  if changed and not player.gui.screen[WINDOW] then
    player.print({ "", text("cycled"), " ", caption(channels.tx(player.index)) })
  end
end

local function dispatcher(handlers)
  return function(event)
    local element = event.element
    if not (element and element.valid and string.sub(element.name or "", 1, #PREFIX) == PREFIX) then return false end
    local tags = element.tags or {}
    local handler = handlers[tags.action]
    if handler then
      local player = game.get_player(event.player_index)
      finish(player, handler(player, tags.arg, element))
    end
    return true
  end
end

settings_gui.on_gui_click = dispatcher(CLICK)
settings_gui.on_gui_elem_changed = dispatcher(ELEM)
settings_gui.on_gui_checked_state_changed = dispatcher(CHECKED)

function settings_gui.on_gui_closed(event)
  local element = event.element
  if not (element and element.valid and element.name == WINDOW) then return false end
  settings_gui.close(game.get_player(event.player_index))
  return true
end

return settings_gui

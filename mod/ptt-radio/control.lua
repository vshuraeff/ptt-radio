-- ptt-radio: push-to-talk voice carried through the multiplayer game.
--
-- each player's companion sends packets to its own game over lua udp. the
-- engine turns a received packet into on_udp_packet_received on every peer,
-- with player_index set to the player whose instance received it (0 = server),
-- so the speaker identity cannot be forged. every peer then forwards voice to
-- the companion of each listening player; send_udp with for_player only fires
-- on that player's own instance, so each machine sends one local copy.
--
-- all state is driven by game.tick: ticks stop during saves and pauses, so a
-- timeout cannot fire on a delivery stall.

local MAGIC = 0x52
local VERSION = 1

local HELLO = 0x01
local START = 0x02
local VOICE = 0x03
local STOP = 0x04
local ROSTER = 0x10
local MIC_STATE = 0x11

local STALE_TICKS = 30
-- the companion sends HELLO every 5 s; three missed heartbeats mean it is gone
local COMPANION_TIMEOUT_TICKS = 15 * 60
local BADGE = "ptt_radio_badge"
local SHORTCUT = "ptt-radio-open-mic"
local SERVER_COLOR = { r = 0.8, g = 0.8, b = 0.8, a = 1 }
local START_SOUND = "utility/gui_click"
local STOP_SOUND = "utility/list_box_click"

local function header(kind, sender)
  return string.char(MAGIC, VERSION, kind, math.floor(sender / 256) % 256, sender % 256)
end

local function companion_port(player)
  return player.mod_settings["ptt-radio-companion-port"].value
end

local function in_scope(sender_index, viewer)
  if sender_index == 0 or settings.global["ptt-radio-scope"].value == "all" then
    return true
  end
  local sender = game.get_player(sender_index)
  return sender ~= nil and sender.force.index == viewer.force.index
end

local function render_badge(viewer)
  local root = viewer.gui.left
  if root[BADGE] then
    root[BADGE].destroy()
  end
  local speakers = {}
  for index in pairs(storage.speakers) do
    if in_scope(index, viewer) then
      speakers[#speakers + 1] = index
    end
  end
  if #speakers == 0 then
    return
  end
  table.sort(speakers)
  local frame = root.add({ type = "frame", name = BADGE, direction = "vertical", caption = { "ptt-radio.title" } })
  for _, index in ipairs(speakers) do
    local speaker = index ~= 0 and game.get_player(index) or nil
    local label = frame.add({ type = "label", caption = speaker and speaker.name or { "ptt-radio.server" } })
    label.style.font = "default-bold"
    if speaker then
      local c = speaker.chat_color
      label.style.font_color = { r = c.r, g = c.g, b = c.b, a = 1 }
    else
      label.style.font_color = SERVER_COLOR
    end
  end
end

local function render_all_badges()
  for _, viewer in pairs(game.connected_players) do
    render_badge(viewer)
  end
end

local function play_for_listeners(sender_index, path)
  for _, viewer in pairs(game.connected_players) do
    if viewer.index ~= sender_index and in_scope(sender_index, viewer) then
      viewer.play_sound({ path = path })
    end
  end
end

local function send_roster(player)
  local lines = { "0\tserver\t204,204,204" }
  for _, p in pairs(game.connected_players) do
    local c = p.chat_color
    lines[#lines + 1] = string.format(
      "%d\t%s\t%d,%d,%d",
      p.index,
      p.name,
      math.floor(c.r * 255 + 0.5),
      math.floor(c.g * 255 + 0.5),
      math.floor(c.b * 255 + 0.5)
    )
  end
  helpers.send_udp(companion_port(player), header(ROSTER, 0) .. table.concat(lines, "\n"), player.index)
end

local function send_roster_to_all()
  for _, player in pairs(game.connected_players) do
    send_roster(player)
  end
end

local function send_mic_state(player)
  local state = storage.open_mic[player.index] and 1 or 0
  helpers.send_udp(companion_port(player), header(MIC_STATE, player.index) .. string.char(state), player.index)
end

local function set_open_mic(player, open)
  storage.open_mic[player.index] = open or nil
  player.set_shortcut_toggled(SHORTCUT, open)
  send_mic_state(player)
end

local function toggle_open_mic(player)
  if not storage.companions[player.index] then
    player.print({ "ptt-radio.no-companion" })
    return
  end
  set_open_mic(player, not storage.open_mic[player.index])
end

local function companion_seen(player, tick)
  local was_online = storage.companions[player.index] ~= nil
  storage.companions[player.index] = tick
  if not was_online then
    player.print({ "ptt-radio.companion-online" })
  end
  send_roster(player)
  send_mic_state(player)
end

local function forget_companion(player_index)
  storage.companions[player_index] = nil
  storage.open_mic[player_index] = nil
end

local function fan_out(sender_index, kind, payload)
  local out = header(kind, sender_index) .. string.sub(payload, 4)
  for _, listener in pairs(game.connected_players) do
    local is_sender = listener.index == sender_index
    if (not is_sender or listener.mod_settings["ptt-radio-echo"].value) and in_scope(sender_index, listener) then
      helpers.send_udp(companion_port(listener), out, listener.index)
    end
  end
end

local function set_speaking(sender_index, speaking)
  local was = storage.speakers[sender_index] ~= nil
  if speaking then
    storage.speakers[sender_index] = game.tick
  else
    storage.speakers[sender_index] = nil
  end
  if was ~= speaking then
    render_all_badges()
    play_for_listeners(sender_index, speaking and START_SOUND or STOP_SOUND)
  end
end

local function init_storage()
  storage.speakers = storage.speakers or {}
  storage.companions = storage.companions or {}
  storage.open_mic = storage.open_mic or {}
end

script.on_init(init_storage)
script.on_configuration_changed(init_storage)

script.on_event(defines.events.on_tick, function()
  helpers.recv_udp()
end)

script.on_event(defines.events.on_udp_packet_received, function(event)
  local payload = event.payload
  local magic, version, kind = string.byte(payload, 1, 3)
  if magic ~= MAGIC or version ~= VERSION then
    return
  end
  local sender_index = event.player_index
  if kind == HELLO then
    if sender_index ~= 0 then
      companion_seen(game.get_player(sender_index), event.tick)
    end
  elseif kind == START or kind == VOICE then
    set_speaking(sender_index, true)
    fan_out(sender_index, kind, payload)
  elseif kind == STOP then
    set_speaking(sender_index, false)
    fan_out(sender_index, kind, payload)
  end
end)

-- a companion that died mid-transmission never sends STOP; close the badge and
-- tell the listeners' companions so their playback stops too
script.on_nth_tick(15, function(event)
  for index, last_tick in pairs(storage.speakers) do
    if event.tick - last_tick > STALE_TICKS then
      set_speaking(index, false)
      fan_out(index, STOP, string.char(MAGIC, VERSION, STOP, 0, 0))
    end
  end
end)

script.on_nth_tick(60, function(event)
  for index, last_tick in pairs(storage.companions) do
    if event.tick - last_tick > COMPANION_TIMEOUT_TICKS then
      local player = game.get_player(index)
      forget_companion(index)
      if player then
        player.set_shortcut_toggled(SHORTCUT, false)
        player.print({ "ptt-radio.companion-offline" })
      end
    end
  end
end)

script.on_event("ptt-radio-toggle-mic", function(event)
  toggle_open_mic(game.get_player(event.player_index))
end)

script.on_event(defines.events.on_lua_shortcut, function(event)
  if event.prototype_name == SHORTCUT then
    toggle_open_mic(game.get_player(event.player_index))
  end
end)

script.on_event(defines.events.on_player_joined_game, function(event)
  local player = game.get_player(event.player_index)
  player.set_shortcut_toggled(SHORTCUT, storage.open_mic[player.index] ~= nil)
  render_badge(player)
  send_roster_to_all()
end)

script.on_event(defines.events.on_player_left_game, function(event)
  if storage.speakers[event.player_index] then
    set_speaking(event.player_index, false)
  end
  forget_companion(event.player_index)
  game.get_player(event.player_index).set_shortcut_toggled(SHORTCUT, false)
  send_roster_to_all()
end)

script.on_event(defines.events.on_player_color_changed, function()
  render_all_badges()
  send_roster_to_all()
end)

script.on_event(defines.events.on_runtime_mod_setting_changed, function(event)
  if event.setting == "ptt-radio-scope" then
    render_all_badges()
  end
end)

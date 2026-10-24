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

local hud = require("hud")

local MAGIC = 0x52
local VERSION = 1

local HELLO = 0x01
local START = 0x02
local VOICE = 0x03
local STOP = 0x04
local ROSTER = 0x10
local MIC_STATE = 0x11
local HALF_DUPLEX = 0x12
local ECHO_STATE = 0x05
local SIGNAL_OUT = 0x06
local START2 = 0x07
local VOICE2 = 0x08
local STOP2 = 0x09
local SIGNAL_IN = 0x13
local AUDIENCE = 0x14
local MAX_SIGNAL_BODY = 1200
local MAX_OPUS = 1164
local MAX_RETIRED_GENERATIONS = 16

local STALE_TICKS = 30
-- the companion sends HELLO every 5 s; three missed heartbeats mean it is gone
local COMPANION_TIMEOUT_TICKS = 15 * 60
-- the gui.left speaker frame of 0.3.0 and earlier, removed on configuration change
local OLD_BADGE = "ptt_radio_badge"
-- every talkspurt is on the common channel until u4 passes the sender's tx key
local CHANNEL = "ptt/any"
local SHORTCUT = "ptt-radio-open-mic"
local ECHO_PANEL = "ptt_radio_echo_panel"
local ECHO_OFF_BUTTON = "ptt_radio_echo_open_mic_off"
local START_SOUND = "utility/gui_click"
local STOP_SOUND = "utility/list_box_click"

local function u16(value)
  return string.char(math.floor(value / 256) % 256, value % 256)
end

local function header(kind, sender)
  return string.char(MAGIC, VERSION, kind) .. u16(sender)
end

local function read_u16(payload, offset)
  local hi, lo = string.byte(payload, offset, offset + 1)
  return hi * 256 + lo
end

local function companion_port(player)
  return player.mod_settings["ptt-radio-companion-port"].value
end

local function send_to_companion(index, data)
  if index == 0 then
    if storage.companions[0] then helpers.send_udp(storage.hub_port, data, 0) end
  else
    local player = game.get_player(index)
    if player and player.connected then helpers.send_udp(companion_port(player), data, index) end
  end
end

local function in_scope(sender_index, viewer)
  if sender_index == 0 or settings.global["ptt-radio-scope"].value == "all" then
    return true
  end
  local sender = game.get_player(sender_index)
  return sender ~= nil and sender.force.index == viewer.force.index
end

hud.visible = function(sender_index, viewer_index)
  return in_scope(sender_index, game.get_player(viewer_index))
end

local function render_all_huds()
  for _, viewer in pairs(game.connected_players) do
    hud.render(viewer)
  end
end

-- personal panel for the echo source only; rebuilt from events, never printed to chat
local function render_echo_panel(player)
  local root = player.gui.left
  if root[ECHO_PANEL] then
    root[ECHO_PANEL].destroy()
  end
  if not storage.echo_sources[player.index] then
    return
  end
  local frame = root.add({ type = "frame", name = ECHO_PANEL, direction = "vertical" })
  local label = frame.add({ type = "label", caption = { "ptt-radio.echo-panel" } })
  label.style.font = "default-bold"
  label.style.font_color = hud.ECHO_COLOR
  label.style.single_line = false
  label.style.maximal_width = 280
  local button = frame.add({ type = "button", name = ECHO_OFF_BUTTON, style = "red_button", caption = { "ptt-radio.echo-open-mic-off" } })
  -- with open mic already off there is nothing to switch off
  button.visible = storage.open_mic[player.index] ~= nil
end

local function set_echo(player_index, on)
  if (storage.echo_sources[player_index] ~= nil) == on then
    return
  end
  storage.echo_sources[player_index] = on or nil
  -- an idle echo source has no hud row; an active one swaps its bars for the marker
  hud.refresh(player_index)
  local player = game.get_player(player_index)
  if player then
    render_echo_panel(player)
  end
end

local function play_for_listeners(sender_index, path)
  for _, viewer in pairs(game.connected_players) do
    if viewer.index ~= sender_index and in_scope(sender_index, viewer) then
      viewer.play_sound({ path = path })
    end
  end
end

local function send_roster(index)
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
  send_to_companion(index, header(ROSTER, 0) .. table.concat(lines, "\n"))
end

local function send_roster_to_all()
  for _, player in pairs(game.connected_players) do
    send_roster(player.index)
  end
  send_roster(0)
end

local function send_audience(index)
  local player = index ~= 0 and game.get_player(index) or nil
  local send_to, receive_from = {}, index == 0 and {} or { 0 }
  for _, other in pairs(game.connected_players) do
    if other.index ~= index then
      if in_scope(index, other) then
        send_to[#send_to + 1] = other.index
      end
      if player and in_scope(other.index, player) then
        receive_from[#receive_from + 1] = other.index
      end
    end
  end
  table.sort(send_to)
  table.sort(receive_from)
  -- fail closed if the roster exceeds the codec's canonical set bound
  if #send_to > 256 or #receive_from > 256 then
    send_to, receive_from = {}, {}
  end
  local body = { storage.audience_revision, u16(#send_to) }
  for _, index in ipairs(send_to) do body[#body + 1] = u16(index) end
  body[#body + 1] = u16(#receive_from)
  for _, index in ipairs(receive_from) do body[#body + 1] = u16(index) end
  send_to_companion(index, header(AUDIENCE, 0) .. table.concat(body))
end

local function audience_changed()
  local bytes = { string.byte(storage.audience_revision, 1, 8) }
  for i = 8, 1, -1 do
    bytes[i] = (bytes[i] + 1) % 256
    if bytes[i] ~= 0 then break end
  end
  storage.audience_revision = string.char(table.unpack(bytes))
  for _, player in pairs(game.connected_players) do
    send_audience(player.index)
  end
  send_audience(0)
end

local function forward_signal(sender_index, payload)
  if #payload < 5 or #payload - 5 > MAX_SIGNAL_BODY then return end
  local target = read_u16(payload, 4)
  local out = header(SIGNAL_IN, sender_index) .. string.sub(payload, 6)
  if (target == 0 or target == 65535) and storage.companions[0] then
    send_to_companion(0, out)
  end
  for _, player in pairs(game.connected_players) do
    if target == player.index or target == 65535 then
      helpers.send_udp(companion_port(player), out, player.index)
    end
  end
end

local function send_mic_state(player)
  local state = storage.open_mic[player.index] and 1 or 0
  helpers.send_udp(companion_port(player), header(MIC_STATE, player.index) .. string.char(state), player.index)
end

-- whether the companion mutes incoming voices while this player holds PTT
local function send_half_duplex(player)
  local state = player.mod_settings["ptt-radio-half-duplex"].value and 1 or 0
  helpers.send_udp(companion_port(player), header(HALF_DUPLEX, player.index) .. string.char(state), player.index)
end

local function set_open_mic(player, open)
  storage.open_mic[player.index] = open or nil
  player.set_shortcut_toggled(SHORTCUT, open)
  send_mic_state(player)
  if storage.echo_sources[player.index] then
    render_echo_panel(player)
  end
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
  send_roster(player.index)
  send_mic_state(player)
  send_half_duplex(player)
  send_audience(player.index)
end

local function forget_companion(player_index)
  storage.companions[player_index] = nil
  storage.v2[player_index] = nil
  storage.open_mic[player_index] = nil
  set_echo(player_index, false)
  storage.talks[player_index] = nil
end

local LEGACY_KIND = { [START2] = START, [VOICE2] = VOICE, [STOP2] = STOP }

-- each listener gets one representation of a sender: a v2 companion gets v2 frames
-- and never a v1 copy of a v2 sender; a legacy companion gets v2 frames as v1, with
-- sequence and opus kept and generation and talkspurt dropped
local function fan_out(sender_index, kind, payload, legacy_only)
  local out = header(kind, sender_index) .. string.sub(payload, 4)
  local legacy_kind = LEGACY_KIND[kind]
  local legacy = legacy_kind and header(legacy_kind, sender_index) .. string.sub(payload, 4, 5) .. string.sub(payload, 16) or out
  local to_v2 = not legacy_only and (legacy_kind ~= nil or not storage.v2[sender_index])
  for _, listener in pairs(game.connected_players) do
    local is_sender = listener.index == sender_index
    if (not is_sender or listener.mod_settings["ptt-radio-echo"].value) and in_scope(sender_index, listener) then
      if not storage.v2[listener.index] then
        helpers.send_udp(companion_port(listener), legacy, listener.index)
      elseif to_v2 then
        helpers.send_udp(companion_port(listener), out, listener.index)
      end
    end
  end
end

local function set_speaking(sender_index, speaking, quiet, restart)
  local was = storage.speakers[sender_index] ~= nil
  if speaking then
    storage.speakers[sender_index] = game.tick
  else
    storage.speakers[sender_index] = nil
  end
  -- only a transition touches the hud, so keepalives never reorder or rebuild rows
  if was ~= speaking then
    if speaking then hud.start(sender_index, CHANNEL) else hud.stop(sender_index) end
    if not quiet then play_for_listeners(sender_index, speaking and START_SOUND or STOP_SOUND) end
  elseif speaking and restart then
    -- a new talkspurt or generation while the old one never closed
    hud.start(sender_index, CHANNEL, true)
  end
end

local function accept_v2(sender_index, kind, payload)
  if (kind == VOICE2 and (#payload < 16 or #payload > 15 + MAX_OPUS))
    or (kind ~= VOICE2 and #payload ~= 15) then return false end
  local generation = string.sub(payload, 6, 13)
  local talkspurt = read_u16(payload, 14)
  local talk = storage.talks[sender_index]
  local repeated = talk and talk.generation == generation and talk.talkspurt == talkspurt and not talk.closed
  if not talk or talk.generation ~= generation then
    if kind == STOP2 or (talk and talk.retired[generation]) then return false end
    local retired = talk and talk.retired or {}
    local count = talk and talk.retired_count or 0
    if talk and count >= MAX_RETIRED_GENERATIONS then return false end
    if talk then retired[talk.generation] = true end
    talk = { generation = generation, talkspurt = talkspurt, retired = retired, retired_count = talk and count + 1 or 0, closed = false }
    storage.talks[sender_index] = talk
  else
    local delta = (talkspurt - talk.talkspurt) % 65536
    if delta >= 32768 or (delta == 0 and talk.closed) then return false end
    if kind == STOP2 and delta ~= 0 then return false end
    if delta > 0 then talk.talkspurt, talk.closed = talkspurt, false end
  end
  talk.last_tick = game.tick
  talk.closed = kind == STOP2
  set_speaking(sender_index, not talk.closed, repeated and kind ~= STOP2, not repeated)
  return true
end

local function init_storage()
  storage.speakers = storage.speakers or {}
  storage.companions = storage.companions or {}
  -- companions whose HELLO announced v2 game media; the rest are legacy
  storage.v2 = storage.v2 or {}
  storage.open_mic = storage.open_mic or {}
  storage.echo_sources = storage.echo_sources or {}
  storage.talks = storage.talks or {}
  storage.audience_revision = storage.audience_revision or string.rep(string.char(0), 8)
  hud.init()
end

script.on_init(init_storage)
script.on_configuration_changed(function()
  init_storage()
  for _, player in pairs(game.players) do
    local badge = player.gui.left[OLD_BADGE]
    if badge then badge.destroy() end
  end
  render_all_huds()
end)

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
  if sender_index ~= 0 then
    local sender = game.get_player(sender_index)
    if not sender or not sender.connected then return end
  end
  if kind == HELLO then
    if sender_index == 0 and #payload == 5 then
      local port = read_u16(payload, 4)
      if port == 0 then port = 34555 end
      if port >= 1024 then
        storage.hub_port = port
        storage.companions[0] = event.tick
        send_roster(0)
        send_audience(0)
      end
    elseif sender_index ~= 0 then
      -- the body is the companion version, followed by the token " v2" when it speaks v2
      storage.v2[sender_index] = string.sub(string.sub(payload, 6), -3) == " v2" or nil
      companion_seen(game.get_player(sender_index), event.tick)
    end
  elseif kind == SIGNAL_OUT then
    forward_signal(sender_index, payload)
  elseif kind == START2 or kind == VOICE2 or kind == STOP2 then
    if accept_v2(sender_index, kind, payload) then fan_out(sender_index, kind, payload) end
  elseif kind == START or kind == VOICE then
    if not storage.talks[sender_index] then
      set_speaking(sender_index, true)
    end
    fan_out(sender_index, kind, payload)
  elseif kind == STOP then
    if not storage.talks[sender_index] then
      set_speaking(sender_index, false)
    end
    fan_out(sender_index, kind, payload)
  elseif kind == ECHO_STATE then
    -- header is 5 bytes; the body is exactly one byte, 0 or 1. only a companion that said
    -- HELLO counts: its timeout is what clears a flag nobody resets
    if sender_index ~= 0 and storage.companions[sender_index] and #payload == 6 then
      local state = string.byte(payload, 6)
      if state == 0 or state == 1 then
        set_echo(sender_index, state == 1)
      end
    end
  end
end)

-- a dead companion leaves a badge behind; legacy playback also needs a stop,
-- while v2 audio may still be arriving on a path independent of game ticks
script.on_nth_tick(15, function(event)
  for index, last_tick in pairs(storage.speakers) do
    if event.tick - last_tick > STALE_TICKS then
      local talk = storage.talks[index]
      set_speaking(index, false, talk ~= nil)
      -- legacy listeners need a timeout stop; v2 listeners are not sent it
      fan_out(index, STOP, string.char(MAGIC, VERSION, STOP, 0, 0), true)
    end
  end
end)

script.on_nth_tick(60, function(event)
  for index, talk in pairs(storage.talks) do
    if event.tick - talk.last_tick > 60 * 60 then
      storage.talks[index] = nil
    end
  end
  for index, last_tick in pairs(storage.companions) do
    if event.tick - last_tick > COMPANION_TIMEOUT_TICKS then
      local player = index ~= 0 and game.get_player(index) or nil
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
  hud.render(player)
  render_echo_panel(player)
  send_roster_to_all()
  audience_changed()
end)

script.on_event(defines.events.on_gui_click, function(event)
  if event.element.valid and event.element.name == ECHO_OFF_BUTTON then
    local player = game.get_player(event.player_index)
    if storage.open_mic[player.index] then
      set_open_mic(player, false)
    end
  end
end)

script.on_event(defines.events.on_player_left_game, function(event)
  if storage.speakers[event.player_index] then
    set_speaking(event.player_index, false)
  end
  forget_companion(event.player_index)
  game.get_player(event.player_index).set_shortcut_toggled(SHORTCUT, false)
  send_roster_to_all()
  audience_changed()
end)

script.on_event(defines.events.on_player_changed_force, function()
  render_all_huds()
  audience_changed()
end)

script.on_event(defines.events.on_forces_merged, function()
  render_all_huds()
  audience_changed()
end)

script.on_event(defines.events.on_player_color_changed, function(event)
  hud.refresh(event.player_index)
  send_roster_to_all()
end)

script.on_event({ defines.events.on_player_display_resolution_changed, defines.events.on_player_display_scale_changed }, function(event)
  hud.reflow(game.get_player(event.player_index))
end)

script.on_event(defines.events.on_runtime_mod_setting_changed, function(event)
  if event.setting == "ptt-radio-scope" then
    render_all_huds()
    audience_changed()
  elseif event.setting == "ptt-radio-half-duplex" and event.player_index then
    local player = game.get_player(event.player_index)
    if storage.companions[player.index] then
      send_half_duplex(player)
    end
  end
end)

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
local channels = require("channels")
local admin = require("admin")
local settings_gui = require("settings_gui")

local MAGIC = 0x52
local VERSION = 1

local HELLO = 0x01
local ROSTER = 0x10
local MIC_STATE = 0x11
local HALF_DUPLEX = 0x12
local ECHO_STATE = 0x05
local SIGNAL_OUT = 0x06
local HUD_STATUS = 0x0A
local START3, VOICE3, STOP3 = 0x0B, 0x0C, 0x0D
local SIGNAL_IN = 0x13
local POLICY = 0x15
local SELF_TEST = 0x16
local SELF_TEST_RESULT = 0x18
local SELF_TEST_RESULT_LEN = 16
local MAX_SIGNAL_BODY = 1200
local MAX_OPUS = 1164
local MAX_POLICY_OPUS = MAX_OPUS - 8
local MAX_RETIRED_GENERATIONS = 16

local STALE_TICKS = 30
-- the companion sends HELLO every 5 s; three missed heartbeats mean it is gone
local COMPANION_TIMEOUT_TICKS = 15 * 60
local SELF_TEST_TICKS = 15 * 60
local COMPANION_NOTICE_TICKS = 10 * 60
-- the gui.left speaker frame of 0.3.0 and earlier, removed on configuration change
local OLD_BADGE = "ptt_radio_badge"
local SHORTCUT = "ptt-radio-open-mic"
local SETTINGS_SHORTCUT = "ptt-radio-settings"
local ECHO_PANEL = "ptt_radio_echo_panel"
local ECHO_OFF_BUTTON = "ptt_radio_echo_open_mic_off"
local START_SOUND = "utility/gui_click"
local STOP_SOUND = "utility/list_box_click"
local set_speaking
local ensure_storage

local function on_event(events, handler)
  script.on_event(events, function(event)
    ensure_storage()
    handler(event)
  end)
end

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

-- SELF_TEST for the requesting player's own companion: header with a zero reserved field + test_id u16
local function encode_self_test(test_id)
  return header(SELF_TEST, 0) .. u16(test_id)
end

-- exactly 16 bytes; the header's reserved field is ignored, the reporter is event.player_index
local function parse_self_test_result(payload)
  if #payload ~= SELF_TEST_RESULT_LEN then return nil end
  if payload:byte(1) ~= MAGIC or payload:byte(2) ~= VERSION or payload:byte(3) ~= SELF_TEST_RESULT then
    return nil
  end
  local outcome = payload:byte(8)
  if outcome > 4 then return nil end
  local peak = payload:byte(10)
  if peak >= 128 then peak = peak - 256 end
  return {
    test_id = read_u16(payload, 6),
    outcome = outcome,
    tone = payload:byte(9),
    mic_peak_dbfs = peak,
    frames_sent = read_u16(payload, 11),
    frames_returned = read_u16(payload, 13),
    first_return_ms = read_u16(payload, 15),
  }
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

hud.visible = function(sender_index, viewer_index)
  local talk = storage.talks[sender_index]
  return channels.eligible(sender_index, viewer_index) and talk and talk.visible and talk.visible[viewer_index] == true
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

local function play_for_listeners(sender_index, path, listeners)
  for _, viewer in pairs(game.connected_players) do
    if viewer.index ~= sender_index and channels.eligible(sender_index, viewer.index)
      and (not listeners or listeners[viewer.index]) then
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

local function valid_index(index)
  return type(index) == "number" and index >= 0 and index <= 65535 and index % 1 == 0
end

local function next_revision(revision)
  local bytes = { string.byte(revision, 1, 8) }
  for i = 8, 1, -1 do
    if bytes[i] < 255 then
      bytes[i] = bytes[i] + 1
      return string.char(table.unpack(bytes))
    end
    bytes[i] = 0
  end
  return nil
end

local function media_ready(index)
  return valid_index(index) and storage.companions[index] ~= nil and storage.pol1[index] == true
end

local function policy_snapshot()
  local snapshot, indices = {}, { 0 }
  for _, player in pairs(game.connected_players) do
    if valid_index(player.index) then indices[#indices + 1] = player.index end
  end
  table.sort(indices)
  for _, index in ipairs(indices) do
    local ready = media_ready(index)
    local row = { send_to = {}, receive_from = {}, receive = {}, hears = {}, tx = ready and channels.tx_permitted(index), channel = channels.tx(index) }
    local player = index ~= 0 and game.get_player(index) or nil
    row.self = row.tx and player ~= nil and (player.mod_settings["ptt-radio-echo"].value
      or storage.self_tests[index] ~= nil)
      and channels.eligible(index, index) or false
    local hud_members = {}
    for _, other in ipairs(indices) do
      if channels.eligible(other, index) then
        row.hears[other] = true
        hud_members[#hud_members + 1] = u16(other)
      end
    end
    row.hud_body = table.concat(hud_members)
    if ready then
      for _, other in ipairs(indices) do
        if other ~= index and media_ready(other) then
          if channels.eligible(index, other) then row.send_to[#row.send_to + 1] = other end
          if channels.eligible(other, index) then row.receive_from[#row.receive_from + 1] = other end
        end
      end
    end
    if #row.send_to > 256 or #row.receive_from > 256 then
      row.send_to, row.receive_from, row.tx, row.self = {}, {}, false, false
    end
    local body = { string.char((row.tx and 1 or 0) + (row.self and 2 or 0)), u16(#row.send_to) }
    for _, other in ipairs(row.send_to) do body[#body + 1] = u16(other) end
    body[#body + 1] = u16(#row.receive_from)
    for _, other in ipairs(row.receive_from) do
      body[#body + 1] = u16(other)
      row.receive[other] = true
    end
    row.body = table.concat(body)
    snapshot[index] = row
  end
  return snapshot
end

local function send_audience(index)
  local row = storage.policy_snapshot and storage.policy_snapshot[index]
  if media_ready(index) and row then
    send_to_companion(index, header(POLICY, 0) .. string.char(1) .. storage.audience_revision .. row.body)
  end
end

local function audience_changed()
  local snapshot, old = policy_snapshot(), storage.policy_snapshot
  local changed = old == nil
  for index, row in pairs(snapshot) do
    local before = old and old[index]
    changed = changed or not before or before.body ~= row.body or before.channel ~= row.channel
      or before.hud_body ~= row.hud_body
  end
  for index in pairs(old or {}) do changed = changed or snapshot[index] == nil end
  if changed then
    local revision = assert(next_revision(storage.audience_revision), "ptt-radio policy revision exhausted")
    storage.previous_policy = old and { revision = storage.audience_revision, players = old } or nil
    storage.policy_snapshot, storage.audience_revision = snapshot, revision
    for index in pairs(storage.speakers) do
      if not snapshot[index] or not snapshot[index].tx then
        set_speaking(index, false, true)
        if storage.talks[index] then storage.talks[index].closed = true end
      end
    end
    for sender, talk in pairs(storage.talks) do
      for viewer in pairs(talk.visible or {}) do
        local row = snapshot[viewer]
        local permitted = row and row.hears[sender]
        if not permitted then
          talk.visible[viewer] = nil
          local links = storage.hud.links[viewer]
          if links then links[sender] = nil end
        end
      end
    end
    -- every epoch reaches every capable companion, even when its own sets are unchanged.
    for _, player in pairs(game.connected_players) do send_audience(player.index) end
    send_audience(0)
    render_all_huds()
  end
  return changed
end

-- a policy change also re-derives every open settings window's disabled controls
admin.configure(function()
  audience_changed()
  for _, player in pairs(game.connected_players) do settings_gui.refresh(player) end
end)
local function finish_self_test(index, status)
  storage.self_tests[index] = nil
  storage.companion_test_status[index] = status
  audience_changed()
  local player = game.get_player(index)
  if player then settings_gui.refresh_companion(player) end
end

local function start_self_test(player)
  local index = player.index
  if storage.self_tests[index] then
    storage.companion_test_status[index] = { "ptt-radio-settings.test-running" }
  elseif not media_ready(index) then
    storage.companion_test_status[index] = { "ptt-radio-settings.test-unavailable" }
  else
    storage.self_test_sequence = (storage.self_test_sequence + 1) % 65536
    storage.self_tests[index] = { id = storage.self_test_sequence, deadline = game.tick + SELF_TEST_TICKS }
    storage.companion_test_status[index] = { "ptt-radio-settings.test-pending" }
    audience_changed()
    send_to_companion(index, encode_self_test(storage.self_test_sequence))
  end
  settings_gui.refresh_companion(player)
end

settings_gui.configure(audience_changed, admin.render, start_self_test)

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
    settings_gui.open(player)
    return
  end
  set_open_mic(player, not storage.open_mic[player.index])
end

local function companion_seen(player, tick)
  local was_online = storage.companions[player.index] ~= nil
  storage.companions[player.index] = tick
  storage.companion_notices[player.index] = nil
  if not was_online then
    player.print({ "ptt-radio.companion-online" })
  end
  send_roster(player.index)
  send_mic_state(player)
  send_half_duplex(player)
end

local function forget_companion(player_index)
  storage.companions[player_index] = nil
  storage.v2[player_index] = nil
  storage.hud1[player_index] = nil
  storage.pol1[player_index] = nil
  storage.open_mic[player_index] = nil
  storage.companion_versions[player_index] = nil
  if storage.self_tests[player_index] then
    storage.self_tests[player_index] = nil
    storage.companion_test_status[player_index] = { "ptt-radio-settings.test-disconnected" }
  end
  set_echo(player_index, false)
  storage.talks[player_index] = nil
  storage.speakers[player_index] = nil
  hud.stop(player_index)
  local player = player_index ~= 0 and game.get_player(player_index) or nil
  if player then
    hud.clear_links(player)
    settings_gui.refresh_companion(player)
  end
end

local function hello_capabilities(player, payload)
  local version = string.match(string.sub(payload, 6), "^ptt%-radio%-companion (%S+)")
  local major, minor, patch = string.match(version or "", "^(%d+)%.(%d+)%.(%d+)$")
  if not major or tonumber(major) > 65535 or tonumber(minor) > 65535 or tonumber(patch) > 65535 then version = nil end
  storage.companion_versions[player.index] = version
  local v2, hud1, pol1
  for token in string.gmatch(string.sub(payload, 6), "%S+") do
    if token == "v2" then v2 = true end
    if token == "hud1" then hud1 = true end
    if token == "pol1" then pol1 = true end
  end
  if storage.v2[player.index] ~= v2 or storage.hud1[player.index] ~= hud1 or storage.pol1[player.index] ~= pol1 then
    hud.clear_links(player)
  end
  local policy_changed = storage.pol1[player.index] ~= pol1
  storage.v2[player.index], storage.hud1[player.index], storage.pol1[player.index] = v2, hud1, pol1
  return policy_changed
end

local function receive_hud_status(reporter, payload)
  if reporter == 0 or not storage.companions[reporter] or not storage.hud1[reporter] or #payload < 7 then return end
  local version, count = string.byte(payload, 6, 7)
  if version ~= 1 or count > 32 or #payload ~= 7 + count * 15 then return end
  -- validate the whole bounded packet before applying even its first record.
  for at = 8, #payload, 15 do
    local level, route, delay_class = string.byte(payload, at + 12, at + 14)
    if level > 3 or route > 3 or delay_class > 3 then return end
  end
  local viewer = game.get_player(reporter)
  for at = 8, #payload, 15 do
    local sender = read_u16(payload, at)
    local talk = storage.talks[sender]
    if storage.speakers[sender] and talk and not talk.closed
      and talk.generation == string.sub(payload, at + 2, at + 9)
      and talk.talkspurt == read_u16(payload, at + 10)
      and hud.visible(sender, reporter) then
      hud.set_link(viewer, sender, string.byte(payload, at + 12, at + 14))
    end
  end
end

local function pair_allowed(snapshot, sender, listener)
  local row = snapshot and snapshot[listener]
  if not row then return false end
  if sender == listener then return row.tx and row.self end
  return row.receive[sender] == true
end

local function fan_out(sender_index, kind, payload, previous)
  local out = header(kind, sender_index) .. string.sub(payload, 4)
  for _, listener in pairs(game.connected_players) do
    if channels.eligible(sender_index, listener.index)
      and pair_allowed(storage.policy_snapshot, sender_index, listener.index)
      and (not previous or pair_allowed(previous, sender_index, listener.index)) then
      helpers.send_udp(companion_port(listener), out, listener.index)
    end
  end
end

set_speaking = function(sender_index, speaking, quiet, restart)
  local was = storage.speakers[sender_index] ~= nil
  if speaking then
    storage.speakers[sender_index] = game.tick
  else
    storage.speakers[sender_index] = nil
  end
  -- only a transition touches the hud, so keepalives never reorder or rebuild rows
  if was ~= speaking then
    if speaking then hud.start(sender_index, storage.talks[sender_index].channel) else hud.stop(sender_index) end
    if not quiet then play_for_listeners(sender_index, speaking and START_SOUND or STOP_SOUND) end
  elseif speaking and restart then
    -- a new talkspurt or generation while the old one never closed
    hud.start(sender_index, storage.talks[sender_index].channel, true)
  elseif speaking then
    hud.start(sender_index, storage.talks[sender_index].channel)
  end
end

local function accept_policy(sender_index, kind, payload)
  if not media_ready(sender_index) then return false end
  if (kind == VOICE3 and (#payload < 24 or #payload > 23 + MAX_POLICY_OPUS))
    or (kind ~= VOICE3 and #payload ~= 23) then return false end
  local current = storage.policy_snapshot
  local sender = current and current[sender_index]
  if not sender or not sender.tx then return false end
  local revision, previous = string.sub(payload, 16, 23), nil
  if revision ~= storage.audience_revision then
    local old = storage.previous_policy
    if not old or old.revision ~= revision or next_revision(old.revision) ~= storage.audience_revision then return false end
    previous = old.players
    if not previous[sender_index] or not previous[sender_index].tx then return false end
  end
  local generation = string.sub(payload, 6, 13)
  local talkspurt = read_u16(payload, 14)
  local talk = storage.talks[sender_index]
  local repeated = talk and talk.generation == generation and talk.talkspurt == talkspurt and not talk.closed
  if not talk or talk.generation ~= generation then
    if kind == STOP3 or (talk and talk.retired[generation]) then return false end
    local retired = talk and talk.retired or {}
    local count = talk and talk.retired_count or 0
    if talk and count >= MAX_RETIRED_GENERATIONS then return false end
    if talk then retired[talk.generation] = true end
    talk = { generation = generation, talkspurt = talkspurt, retired = retired, retired_count = talk and count + 1 or 0, closed = false, visible = {}, stopped = {} }
    storage.talks[sender_index] = talk
  else
    local delta = (talkspurt - talk.talkspurt) % 65536
    if delta >= 32768 or (delta == 0 and talk.closed) then return false end
    if kind == STOP3 and delta ~= 0 then return false end
    if delta > 0 then
      talk.talkspurt, talk.closed, talk.visible, talk.stopped, talk.channel = talkspurt, false, {}, {}, nil
    end
  end
  local activated, stopped = {}, {}
  for _, player in pairs(game.connected_players) do
    local index = player.index
    -- visual/click eligibility includes viewers without a voice-capable companion.
    local allowed = current[index] and current[index].hears[sender_index]
      and (not previous or previous[index] and previous[index].hears[sender_index])
    if allowed then
      if kind == STOP3 then
        if talk.visible[index] then stopped[index] = true end
        talk.visible[index], talk.stopped[index] = nil, true
      elseif not talk.stopped[index] then
        if not talk.visible[index] then activated[index] = true end
        talk.visible[index] = true
      end
    end
  end
  if not previous or not talk.channel then
    talk.channel = previous and previous[sender_index].channel or sender.channel
  end
  talk.last_tick = game.tick
  talk.closed = kind == STOP3 and next(talk.visible) == nil
  set_speaking(sender_index, next(talk.visible) ~= nil, true, not repeated)
  if next(activated) then play_for_listeners(sender_index, START_SOUND, activated) end
  if next(stopped) then play_for_listeners(sender_index, STOP_SOUND, stopped) end
  for _, player in pairs(game.connected_players) do
    if activated[player.index] or stopped[player.index] then hud.render(player) end
  end
  return true, previous
end

local function init_storage()
  storage.speakers = storage.speakers or {}
  storage.companions = storage.companions or {}
  -- companions whose HELLO announced v2 game media; the rest are legacy
  storage.v2 = storage.v2 or {}
  storage.hud1 = storage.hud1 or {}
  storage.pol1 = storage.pol1 or {}
  storage.open_mic = storage.open_mic or {}
  storage.companion_versions = storage.companion_versions or {}
  storage.companion_notices = storage.companion_notices or {}
  storage.self_tests = storage.self_tests or {}
  storage.self_test_sequence = storage.self_test_sequence or 0
  storage.companion_test_status = storage.companion_test_status or {}
  storage.echo_sources = storage.echo_sources or {}
  storage.talks = storage.talks or {}
  storage.audience_revision = storage.audience_revision or string.rep(string.char(0), 8)
  if not storage.channel_policy_active then
    storage.channel_policy_active = true
    storage.speakers, storage.talks, storage.hud = {}, {}, nil
    storage.policy_snapshot, storage.previous_policy = nil, nil
  end
  storage.physical_surfaces = storage.physical_surfaces or {}
  for _, player in pairs(game.connected_players) do
    storage.physical_surfaces[player.index] = player.physical_surface.index
    if not storage.companions[player.index] then
      storage.companion_notices[player.index] = game.tick + COMPANION_NOTICE_TICKS
    end
  end
  hud.init()
  channels.init()
  settings_gui.init()
end

ensure_storage = function()
  if not storage.channel_policy_active then
    init_storage()
    render_all_huds()
    audience_changed()
  end
end

admin.register(ensure_storage)
script.on_init(init_storage)
script.on_configuration_changed(function()
  init_storage()
  for _, player in pairs(game.players) do
    local badge = player.gui.left[OLD_BADGE]
    if badge then badge.destroy() end
  end
  render_all_huds()
  audience_changed()
end)

on_event(defines.events.on_tick, function(event)
  helpers.recv_udp()
  hud.expire(event.tick)
  for index, pending in pairs(storage.self_tests) do
    if event.tick >= pending.deadline then
      finish_self_test(index, { "ptt-radio-settings.test-timeout" })
    end
  end
  for index, deadline in pairs(storage.companion_notices) do
    if event.tick >= deadline then
      storage.companion_notices[index] = nil
      local player = game.get_player(index)
      if player and player.connected and not storage.companions[index] then
        player.print({ "ptt-radio-settings.companion-notice" })
        settings_gui.open(player)
      end
    end
  end
end)

on_event(defines.events.on_udp_packet_received, function(event)
  local payload = event.payload
  local magic, version, kind = string.byte(payload, 1, 3)
  if #payload < 5 or magic ~= MAGIC or version ~= VERSION then
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
      local player = game.get_player(sender_index)
      local changed = hello_capabilities(player, payload) or not storage.companions[player.index]
      if not storage.pol1[player.index] and storage.self_tests[player.index] then
        storage.self_tests[player.index] = nil
        storage.companion_test_status[player.index] = { "ptt-radio-settings.test-disconnected" }
      end
      companion_seen(player, event.tick)
      if not ((changed or not storage.policy_snapshot) and audience_changed()) then send_audience(player.index) end
      if changed then settings_gui.refresh(player) end
      settings_gui.refresh_companion(player)
    end
  elseif kind == SELF_TEST_RESULT then
    local pending = storage.self_tests[sender_index]
    local result = pending and parse_self_test_result(payload)
    if result and result.test_id == pending.id and event.tick < pending.deadline then
      finish_self_test(sender_index, { "ptt-radio-settings.test-result",
        { "ptt-radio-settings.test-outcome-" .. result.outcome },
        { result.tone == 1 and "ptt-radio-settings.test-tone-yes" or "ptt-radio-settings.test-tone-no" },
        result.mic_peak_dbfs == -128 and { "ptt-radio-settings.test-no-capture" } or result.mic_peak_dbfs,
        result.frames_sent, result.frames_returned, result.first_return_ms })
    end
  elseif kind == HUD_STATUS then
    receive_hud_status(sender_index, payload)
  elseif kind == SIGNAL_OUT then
    forward_signal(sender_index, payload)
  elseif kind == START3 or kind == VOICE3 or kind == STOP3 then
    local accepted, previous = accept_policy(sender_index, kind, payload)
    if accepted then fan_out(sender_index, kind, payload, previous) end
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
  ensure_storage()
  for index, last_tick in pairs(storage.speakers) do
    if event.tick - last_tick > STALE_TICKS then
      local talk = storage.talks[index]
      set_speaking(index, false, talk ~= nil)
    end
  end
end)

script.on_nth_tick(60, function(event)
  ensure_storage()
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
      audience_changed()
    end
  end
end)

on_event("ptt-radio-toggle-mic", function(event)
  toggle_open_mic(game.get_player(event.player_index))
end)

on_event("ptt-radio-settings-open", function(event)
  settings_gui.toggle(game.get_player(event.player_index))
end)

on_event("ptt-radio-settings-cycle", function(event)
  settings_gui.cycle(game.get_player(event.player_index))
end)

on_event(defines.events.on_lua_shortcut, function(event)
  if event.prototype_name == SHORTCUT then
    toggle_open_mic(game.get_player(event.player_index))
  elseif event.prototype_name == SETTINGS_SHORTCUT then
    settings_gui.toggle(game.get_player(event.player_index))
  end
end)

on_event(defines.events.on_player_joined_game, function(event)
  local player = game.get_player(event.player_index)
  forget_companion(player.index)
  storage.companion_notices[player.index] = game.tick + COMPANION_NOTICE_TICKS
  player.set_shortcut_toggled(SHORTCUT, storage.open_mic[player.index] ~= nil)
  hud.render(player)
  render_echo_panel(player)
  storage.physical_surfaces[player.index] = player.physical_surface.index
  send_roster_to_all()
  audience_changed()
end)

on_event(defines.events.on_gui_click, function(event)
  if admin.on_gui_click(event) or settings_gui.on_gui_click(event) then return end
  if event.element.valid and event.element.name == ECHO_OFF_BUTTON then
    local player = game.get_player(event.player_index)
    if storage.open_mic[player.index] then
      set_open_mic(player, false)
    end
  end
end)

-- one handler per event: a second script.on_event would replace the first
on_event(defines.events.on_gui_elem_changed, function(event)
  if not admin.on_gui_elem_changed(event) then settings_gui.on_gui_elem_changed(event) end
end)
on_event(defines.events.on_gui_checked_state_changed, settings_gui.on_gui_checked_state_changed)
on_event(defines.events.on_gui_closed, settings_gui.on_gui_closed)
on_event(defines.events.on_string_translated, settings_gui.on_string_translated)

on_event(defines.events.on_player_left_game, function(event)
  if storage.speakers[event.player_index] then
    set_speaking(event.player_index, false)
  end
  forget_companion(event.player_index)
  storage.companion_notices[event.player_index] = nil
  game.get_player(event.player_index).set_shortcut_toggled(SHORTCUT, false)
  send_roster_to_all()
  audience_changed()
end)

on_event(defines.events.on_player_changed_force, function()
  audience_changed()
end)

on_event(defines.events.on_forces_merged, function()
  audience_changed()
end)

on_event(defines.events.on_player_changed_surface, function(event)
  local player = game.get_player(event.player_index)
  local surface = player.physical_surface.index
  if storage.physical_surfaces[player.index] ~= surface then
    storage.physical_surfaces[player.index] = surface
    audience_changed()
  end
end)

on_event({ defines.events.on_player_promoted, defines.events.on_player_demoted }, function()
  audience_changed()
  for _, player in pairs(game.connected_players) do settings_gui.refresh(player) end
end)

on_event(defines.events.on_player_color_changed, function(event)
  hud.refresh(event.player_index)
  send_roster_to_all()
end)

on_event({ defines.events.on_player_display_resolution_changed, defines.events.on_player_display_scale_changed }, function(event)
  hud.reflow(game.get_player(event.player_index))
end)

on_event(defines.events.on_runtime_mod_setting_changed, function(event)
  if event.setting == "ptt-radio-scope" then
    audience_changed()
  elseif event.setting == "ptt-radio-echo" then
    audience_changed()
  elseif event.setting == "ptt-radio-half-duplex" and event.player_index then
    local player = game.get_player(event.player_index)
    if storage.companions[player.index] then
      send_half_duplex(player)
    end
  end
end)

-- the self-test codec helpers, returned so the unit tests can call them
return {
  encode_self_test = encode_self_test,
  parse_self_test_result = parse_self_test_result,
}

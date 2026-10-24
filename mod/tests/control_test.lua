local handlers, nth, sent, sounds, players
local control_path = arg[1] or "ptt-radio/mod/ptt-radio/control.lua"
package.path = (control_path:gsub("[^/]*$", "?.lua")) .. ";" .. package.path
-- a gui element keeps its add spec, its children in order and named children as fields
local function element(parent, spec)
  local item = { valid = true, children = {} }
  for key, value in pairs(spec or {}) do item[key] = value end
  item.style = {}
  item.add = function(child_spec)
    local child = element(item, child_spec)
    if child.name then item[child.name] = child end
    item.children[#item.children + 1] = child
    return child
  end
  item.destroy = function()
    item.valid = false
    if item.name then parent[item.name] = nil end
    for i, child in ipairs(parent.children) do
      if child == item then table.remove(parent.children, i) break end
    end
  end
  return item
end

local function setup()
  handlers, nth, sent, sounds, players = {}, {}, {}, {}, {}
  storage = {}
  commands = { add_command = function(name, _, handler) commands[name] = handler end }
  settings = { global = { ["ptt-radio-scope"] = { value = "all" } } }
  defines = { events = setmetatable({}, { __index = function(_, key) return key end }) }
  script = {
    on_event = function(event, handler)
      for _, name in ipairs(type(event) == "table" and event or { event }) do handlers[name] = handler end
    end,
    on_nth_tick = function(tick, handler) nth[tick] = handler end,
    on_init = function(handler) script.init = handler end,
    on_configuration_changed = function(handler) script.configure = handler end,
  }
  helpers = {
    send_udp = function(port, data, target) sent[#sent + 1] = { port = port, data = data, target = target } end,
    recv_udp = function() end,
  }
  game = {
    print = function() end,
    tick = 0,
    connected_players = {},
    players = players,
    get_player = function(index) assert(index ~= 0, "player zero is not a LuaPlayer"); return players[index] end,
  }
  for index = 1, 3 do
    players[index] = {
      index = index, connected = true, name = "p" .. index, force = { index = 1 },
      chat_color = { r = 1, g = 0, b = 0 },
      gui = { left = element({ children = {} }), screen = element({ children = {} }) },
      display_resolution = { width = 1920, height = 1080 }, display_scale = 1,
      mod_settings = {
        ["ptt-radio-companion-port"] = { value = 34555 + index },
        ["ptt-radio-half-duplex"] = { value = true },
        ["ptt-radio-echo"] = { value = false },
      },
      print = function() end, set_shortcut_toggled = function() end,
      play_sound = function(spec) sounds[#sounds + 1] = spec.path end,
    }
  end
  game.connected_players = { players[3], players[1], players[2] }
  package.loaded.hud = nil
  package.loaded.channels = nil
  dofile(control_path)
  script.init()
end

local function u16(n) return string.char(math.floor(n / 256), n % 256) end
local function packet(kind, peer, body) return string.char(0x52, 1, kind) .. u16(peer or 0) .. (body or "") end
local function emit(sender, data, tick)
  game.tick = tick or game.tick
  handlers.on_udp_packet_received({ player_index = sender, payload = data, tick = game.tick })
end
local function clear() sent = {} end
local function audience(index)
  for i = #sent, 1, -1 do
    local row = sent[i]
    if row.target == index and row.data:byte(3) == 0x14 then
      local data, at = row.data, 14
      local function read()
        local value = data:byte(at) * 256 + data:byte(at + 1)
        at = at + 2
        return value
      end
      local function set()
        local out = {}
        for _ = 1, read() do out[#out + 1] = read() end
        return table.concat(out, ",")
      end
      local send_to, receive_from = set(), set()
      assert(at == #data + 1)
      return send_to, receive_from, data:sub(6, 13)
    end
  end
  error("missing audience for " .. index)
end
local function expect_audience(index, send_to, receive_from)
  local actual_send, actual_receive = audience(index)
  assert(actual_send == send_to, actual_send .. " != " .. send_to)
  assert(actual_receive == receive_from, actual_receive .. " != " .. receive_from)
end
local function v2(kind, generation, talkspurt, body)
  return packet(kind, 42, generation .. u16(talkspurt) .. (body or ""))
end

setup()
emit(1, packet(1))
expect_audience(1, "2,3", "0,2,3")
clear()
emit(2, packet(1))
expect_audience(2, "1,3", "0,1,3")
assert(storage.companions[1] == 0)
players[3].force.index = 2
settings.global["ptt-radio-scope"].value = "force"
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" })
expect_audience(1, "2", "0,2")
expect_audience(3, "", "0")
local _, _, revision = audience(1)
players[2].force.index = 2
handlers.on_player_changed_force({ player_index = 2 })
expect_audience(1, "", "0")
expect_audience(2, "3", "0,3")
assert(select(3, audience(1)) ~= revision)
players[2].force = players[1].force
players[3].force = players[1].force
handlers.on_forces_merged({ source_name = "invalidated" })
expect_audience(1, "2,3", "0,2,3")
players[2].connected = false
game.connected_players = { players[3], players[1] }
handlers.on_player_left_game({ player_index = 2 })
expect_audience(1, "3", "0,3")
players[2].connected = true
game.connected_players = { players[3], players[1], players[2] }
handlers.on_player_joined_game({ player_index = 2 })
expect_audience(1, "2,3", "0,2,3")

setup()
players[3].force.index = 2
settings.global["ptt-radio-scope"].value = "force"
emit(1, packet(6, 3, "opaque\0signal"))
assert(#sent == 1 and sent[1].target == 3 and sent[1].data == packet(0x13, 1, "opaque\0signal"))
clear()
emit(1, packet(6, 0, "hub"))
assert(#sent == 0)
emit(0, packet(1, 45678))
expect_audience(0, "1,2,3", "")
assert(sent[1].target == 0 and sent[1].port == 45678)
clear()
emit(1, packet(6, 0, "hub"))
assert(#sent == 1 and sent[1].target == 0 and sent[1].data == packet(0x13, 1, "hub"))
clear()
emit(0, packet(6, 2, "from hub"))
assert(#sent == 1 and sent[1].data == packet(0x13, 0, "from hub"))
clear()
emit(2, packet(6, 65535, string.rep("x", 1200)))
assert(#sent == 4)
for _, row in ipairs(sent) do assert(row.data:byte(3) == 0x13 and #row.data == 1205) end
clear()
for _, bad in ipairs({ "", "R", string.char(0x51, 1, 6, 0, 3), string.char(0x52, 2, 6, 0, 3), packet(6, 2, string.rep("x", 1201)), string.char(0x52, 1, 6, 0), packet(0x14, 2, "forged"), packet(0x13, 2, "forged"), packet(6, 42, "absent") }) do
  emit(1, bad)
end
assert(#sent == 0)
local nested = packet(0x14, 0, "forged audience")
emit(1, packet(6, 3, nested))
assert(#sent == 1 and sent[1].data == packet(0x13, 1, nested))
clear()
emit(0, packet(1, 0))
assert(sent[1].port == 34555)
clear()
emit(0, packet(1, 100))
emit(0, packet(1, 45678, "extra"))
assert(#sent == 0 and storage.hub_port == 34555)
game.tick = 900
nth[60]({ tick = game.tick })
emit(1, packet(6, 0, "boundary"))
assert(#sent == 1 and sent[1].target == 0)
clear()
game.tick = 960
nth[60]({ tick = game.tick })
emit(1, packet(6, 0, "expired"))
assert(#sent == 0)

setup()
local gen = string.char(255, 254, 253, 252, 251, 250, 249, 248)
local gen2 = string.char(255, 254, 253, 252, 251, 250, 249, 249)
emit(1, v2(9, gen, 1))
assert(storage.talks[1] == nil and #sent == 0)
emit(1, v2(7, gen, 1))
assert(#sent == 2 and #sounds == 2 and storage.speakers[1] == 0)
-- no listener said v2 in HELLO: both get the legacy START
assert(sent[1].data == packet(2, 1, u16(42)) and sent[2].data == sent[1].data)
emit(1, v2(7, gen, 1), 15)
assert(#sounds == 2 and storage.speakers[1] == 15)
emit(1, packet(3, 43, "legacy"), 16)
assert(storage.speakers[1] == 15)
clear()
game.tick = 60
nth[15]({ tick = 60 })
assert(storage.speakers[1] == nil and #sent == 2 and #sounds == 2)
assert(not storage.talks[1].closed and storage.talks[1].talkspurt == 1)
for _, row in ipairs(sent) do assert(row.data == packet(4, 1, u16(0))) end
emit(1, v2(7, gen, 1), 61)
assert(storage.speakers[1] == 61 and #sounds == 2)
emit(1, v2(8, gen, 1, "opus"), 62)
assert(storage.speakers[1] == 62)
emit(1, v2(7, gen, 2), 63)
clear()
emit(1, v2(9, gen, 1), 64)
emit(1, v2(7, gen, 1), 64)
assert(#sent == 0 and storage.speakers[1] == 63)
emit(1, packet(4), 65)
assert(#sent == 2 and storage.speakers[1] == 63)
emit(1, v2(9, gen, 2), 66)
assert(storage.speakers[1] == nil)
clear()
emit(1, v2(7, gen, 2), 67)
assert(#sent == 0)
emit(1, v2(7, gen2, 1), 68)
assert(storage.talks[1].generation == gen2)
clear()
emit(1, v2(8, gen, 3, "old"), 69)
emit(1, v2(9, gen, 3), 69)
assert(#sent == 0 and storage.talks[1].generation == gen2)
for _, bad in ipairs({ v2(7, gen2, 2):sub(1, 14), v2(7, gen2, 2) .. "x", v2(9, gen2, 1) .. "x", v2(8, gen2, 1), v2(8, gen2, 1, string.rep("x", 1165)) }) do
  emit(1, bad)
end
assert(#sent == 0)
emit(1, v2(8, gen2, 1, string.rep("x", 1164)))
assert(#sent == 2)

setup()
emit(1, v2(7, gen, 65535))
emit(1, v2(7, gen, 0), 1)
assert(storage.talks[1].talkspurt == 0 and storage.speakers[1] == 1)
clear()
emit(1, v2(9, gen, 65535), 2)
emit(1, v2(7, gen, 65535), 2)
assert(#sent == 0 and storage.talks[1].talkspurt == 0)

setup()
for i = 1, 17 do emit(1, v2(7, string.rep("\0", 7) .. string.char(i), 1), i) end
assert(storage.talks[1].retired_count == 16)
clear()
emit(1, v2(7, string.rep("\0", 7) .. string.char(18), 1), 18)
assert(#sent == 0)
emit(1, packet(1), 3600)
assert(storage.talks[1].retired_count == 16)
game.tick = 3660
nth[60]({ tick = 3660 })
assert(storage.talks[1] == nil and storage.companions[1] == 3600)
clear()
emit(1, v2(7, string.rep("\0", 7) .. string.char(18), 1), 3661)
assert(#sent == 2)

-- one representation per listener: player 1 and 2 say v2, player 3 is legacy
setup()
local function to(index)
  local rows = {}
  for _, row in ipairs(sent) do
    if row.target == index then rows[#rows + 1] = row.data end
  end
  return rows
end
local function only(index, data)
  local rows = to(index)
  assert(#rows == 1 and rows[1] == data, "listener " .. index .. " got " .. #rows)
end
for _, body in ipairs({ "", "0.4.0", "0.4.0v2", "0.4.0 v2 ", "0.4.0 V2", "v2" }) do
  emit(1, packet(1, 0, body))
  assert(storage.v2[1] == nil, body)
end
-- the token is read from the body, not the header
emit(1, packet(1, 32, "v2"))
assert(storage.v2[1] == nil)
emit(1, packet(1, 0, " v2"))
assert(storage.v2[1] == true)
emit(1, packet(1, 0, "0.4.0"))
assert(storage.v2[1] == nil)
emit(0, packet(1, 45678))
assert(storage.v2[0] == nil and storage.companions[0] == 0)
emit(1, packet(1, 0, "ptt-radio-companion 0.4.0 v2"))
emit(2, packet(1, 0, "ptt-radio-companion 0.4.0 v2"))
emit(3, packet(1, 0, "ptt-radio-companion 0.4.0"))
assert(storage.v2[1] and storage.v2[2] and storage.v2[3] == nil)
clear()
emit(1, v2(7, gen, 1))
only(2, string.char(0x52, 1, 7, 0, 1, 0, 42) .. gen .. u16(1))
only(3, string.char(0x52, 1, 2, 0, 1, 0, 42))
assert(#to(1) == 0 and #to(0) == 0)
clear()
emit(1, v2(8, gen, 1, "opus"))
only(2, string.char(0x52, 1, 8, 0, 1, 0, 42) .. gen .. u16(1) .. "opus")
only(3, string.char(0x52, 1, 3, 0, 1, 0, 42) .. "opus")
clear()
-- a v1 copy from a v2 sender reaches legacy listeners only
emit(1, packet(3, 44, "copy"))
assert(#to(2) == 0)
only(3, packet(3, 1, u16(44) .. "copy"))
clear()
-- a legacy sender reaches every listener unchanged
emit(3, packet(3, 7, "legacy"))
only(1, packet(3, 3, u16(7) .. "legacy"))
only(2, packet(3, 3, u16(7) .. "legacy"))
clear()
emit(1, v2(9, gen, 1))
only(2, string.char(0x52, 1, 9, 0, 1, 0, 42) .. gen .. u16(1))
only(3, string.char(0x52, 1, 4, 0, 1, 0, 42))
clear()
-- self-echo: a v2 sender hears its own v2 frame and no v1 copy; a legacy one hears v1
players[1].mod_settings["ptt-radio-echo"].value = true
players[3].mod_settings["ptt-radio-echo"].value = true
emit(1, v2(7, gen, 2), 1)
only(1, string.char(0x52, 1, 7, 0, 1, 0, 42) .. gen .. u16(2))
only(3, packet(2, 1, u16(42)))
clear()
emit(1, packet(2, 45))
assert(#to(1) == 0 and #to(2) == 0)
only(3, packet(2, 1, u16(45)))
clear()
emit(3, v2(7, gen2, 1), 1)
only(3, packet(2, 3, u16(42)))
only(1, string.char(0x52, 1, 7, 0, 3, 0, 42) .. gen2 .. u16(1))
clear()
-- the synthetic badge-timeout STOP goes to legacy listeners only, self-echo included
game.tick = 60
nth[15]({ tick = 60 })
assert(storage.speakers[1] == nil and storage.speakers[3] == nil)
assert(#to(1) == 0 and #to(2) == 0 and #sent == 2)
local stops = { [to(3)[1]] = true, [to(3)[2]] = true }
assert(stops[packet(4, 1, u16(0))] and stops[packet(4, 3, u16(0))])
-- capability goes with the companion record: on leave and on HELLO timeout
players[2].connected = false
game.connected_players = { players[3], players[1] }
handlers.on_player_left_game({ player_index = 2 })
assert(storage.v2[2] == nil and storage.v2[1])
game.tick = 960
nth[60]({ tick = game.tick })
assert(storage.v2[1] == nil and storage.companions[1] == nil)

setup()
storage.audience_revision = string.rep("\0", 7) .. string.char(255)
handlers.on_player_changed_force({ player_index = 1 })
assert(select(3, audience(1)) == string.rep("\0", 6) .. string.char(1, 0))
game.connected_players = {}
for i = 1, 257 do
  local player = setmetatable({ index = i }, { __index = players[1] })
  game.connected_players[i] = player
  players[i] = player
end
clear()
emit(1, packet(1))
expect_audience(1, "", "")

-- a 0.2.0 save holds only the old tables; the version bump runs on_configuration_changed
setup()
storage = { speakers = {}, companions = {}, open_mic = {} }
script.configure({})
emit(1, packet(1))
expect_audience(1, "2,3", "0,2,3")
handlers.on_player_joined_game({ player_index = 2 })
assert(storage.v2 and storage.echo_sources and storage.talks and #storage.audience_revision == 8)
assert(storage.radio and storage.radio_policy.deny)

-- speaker hud: a viewer's stack as row senders in screen order, «ещё N» as +N
local function hud_root(index) return players[index].gui.screen.ptt_radio_hud end
local function stack(index)
  local root, out = hud_root(index), {}
  for _, row in ipairs(root and root.children or {}) do
    out[#out + 1] = row.name and row.name:sub(#"ptt_radio_hud_" + 1) or "+" .. row.content.children[1].caption[2]
  end
  return table.concat(out, ",")
end
local function content(viewer, sender) return hud_root(viewer)["ptt_radio_hud_" .. sender].content end
local function tail(viewer, sender) return content(viewer, sender).tail end

-- append order, a third talker, compaction, late STOPs and the timeout
setup()
emit(2, v2(7, gen, 1), 1)
emit(1, v2(7, gen2, 1), 2)
assert(stack(1) == "2,1" and stack(2) == "2,1" and stack(3) == "2,1")
local root = hud_root(3)
assert(root.location.x == 1920 - 248 and root.location.y == 432)
local icon = content(3, 2).children[1]
assert(icon.type == "sprite" and icon.sprite == "virtual-signal/signal-each")
-- keepalives and a repeated START neither reorder nor rebuild
emit(2, v2(8, gen, 1, "opus"), 3)
emit(2, v2(7, gen, 1), 3)
emit(1, v2(8, gen2, 1, "opus"), 3)
assert(hud_root(3) == root and root.valid and stack(3) == "2,1")
emit(3, packet(2, 5), 4)
assert(stack(1) == "2,1,3" and stack(3) == "2,1,3")
emit(2, v2(9, gen, 1), 5)
assert(stack(1) == "1,3" and #hud_root(1).children == 2)
assert(hud_root(1).location.y == 432)
-- back with a new talkspurt: to the end; the late STOP2 of the old one is rejected
emit(2, v2(7, gen, 2), 6)
assert(stack(1) == "1,3,2")
emit(2, v2(9, gen, 1), 7)
-- a STOP2 of another generation and a v1 STOP from a v2 talker are rejected too
emit(1, v2(9, gen, 1), 7)
emit(1, packet(4), 7)
assert(stack(1) == "1,3,2" and stack(3) == "1,3,2")
-- the tick timeout drops a lost STOP: player 1 last spoke at 3, player 3 at 4, player 2 at 6
game.tick = 34
nth[15]({ tick = 34 })
assert(stack(1) == "3,2")
game.tick = 35
nth[15]({ tick = 35 })
assert(stack(1) == "2")
emit(2, v2(9, gen, 2), 36)
assert(hud_root(1) == nil and hud_root(2) == nil and #storage.hud.rows == 0)

-- a new talkspurt or generation whose predecessor never closed moves the row to the end and resets its links
setup()
local hud_mod = require("hud")
emit(1, v2(7, gen, 1), 1)
emit(2, v2(7, gen2, 1), 2)
assert(stack(3) == "1,2")
hud_mod.set_link(players[3], 1, 2, 1, 1)
hud_mod.set_link(players[3], 2, 2, 1, 1)
hud_mod.set_link(players[1], 1, 2, 1, 1)
local row_a, row_b = hud_root(3).ptt_radio_hud_1, hud_root(3).ptt_radio_hud_2
-- identical START, START2 and VOICE of the same identity keep order and elements
emit(1, v2(7, gen, 1), 3)
emit(1, v2(8, gen, 1, "opus"), 3)
emit(2, v2(7, gen2, 1), 3)
assert(stack(3) == "1,2" and hud_root(3).ptt_radio_hud_1 == row_a and hud_root(3).ptt_radio_hud_2 == row_b)
assert(storage.hud.links[3][1] and storage.hud.links[3][2])
-- A's STOP is lost and a second talkspurt starts: order is B, A; only A's links reset
emit(1, v2(7, gen, 2), 4)
assert(stack(3) == "2,1" and stack(1) == "2,1" and stack(2) == "2,1")
assert(storage.hud.links[3][1] == nil and storage.hud.links[1][1] == nil)
assert(storage.hud.links[3][2] and hud_root(3).ptt_radio_hud_2.valid)
assert(#storage.hud.rows == 2 and storage.speakers[1])
-- a repeat of the new talkspurt again changes nothing
local moved_a = hud_root(3).ptt_radio_hud_1
emit(1, v2(8, gen, 2, "opus"), 5)
emit(1, v2(7, gen, 2), 5)
assert(stack(3) == "2,1" and hud_root(3).ptt_radio_hud_1 == moved_a)
-- a new stream generation of B while its row exists moves it to the end as well
hud_mod.set_link(players[3], 2, 2, 1, 1)
emit(2, v2(7, gen2:sub(1, 7) .. "\2", 1), 6)
assert(stack(3) == "1,2" and storage.hud.links[3][2] == nil)

-- a viewer outside the scope gets no row
setup()
players[3].force = { index = 2 }
settings.global["ptt-radio-scope"].value = "force"
emit(1, v2(7, gen, 1), 1)
assert(stack(1) == "1" and stack(2) == "1" and hud_root(3) == nil)

-- overflow and reflow, with the server as a fourth talker
setup()
players[1].display_resolution = { width = 1280, height = 300 }
emit(1, v2(7, gen, 1), 1)
emit(2, v2(7, gen, 1), 1)
emit(3, packet(2, 5), 1)
emit(0, packet(2, 5), 1)
assert(stack(1) == "1,2,+2" and stack(2) == "1,2,3,0")
assert(hud_root(1).location.x == 1280 - 248 and hud_root(1).location.y == 120)
local other = hud_root(2)
players[1].display_resolution = { width = 1920, height = 1080 }
handlers.on_player_display_resolution_changed({ player_index = 1 })
assert(stack(1) == "1,2,3,0" and hud_root(1).location.x == 1672)
assert(content(1, 0).children[2].caption[1] == "ptt-radio.server")
players[1].display_scale = 2
handlers.on_player_display_scale_changed({ player_index = 1 })
assert(stack(1) == "1,2,3,0" and hud_root(1).location.x == 1920 - 496 and hud_root(1).location.y == 432)
players[1].display_resolution = { width = 1920, height = 600 }
handlers.on_player_display_resolution_changed({ player_index = 1 })
assert(stack(1) == "1,2,+2" and hud_root(1).location.y == 240)
-- reflowing one viewer leaves the others alone
assert(hud_root(2) == other and other.valid)

-- an idle echo source has no row; an active one shows the marker instead of the bars
setup()
emit(2, packet(1), 0)
emit(2, packet(5, 0, "\1"), 1)
assert(storage.echo_sources[2] and hud_root(1) == nil and hud_root(2) == nil)
assert(players[2].gui.left.ptt_radio_echo_panel)
emit(2, v2(7, gen, 1), 2)
local marker = tail(1, 2)
assert(marker.bars == nil and marker.children[1].tooltip[1] == "ptt-radio.echo-tooltip")
emit(2, packet(5, 0, "\0"), 3)
local bars = tail(1, 2).bars
assert(#bars.children == 3 and players[2].gui.left.ptt_radio_echo_panel == nil)
-- the bars stay grey and unknown until a link report, which repaints them in place
assert(bars.tooltip[1] == "ptt-radio.hud-link-unknown")
for _, segment in ipairs(bars.children) do assert(segment.value == 1 and segment.style.color.r == 0.45) end
local hud = require("hud")
hud.set_link(players[1], 2, 2, 1, 1)
assert(tail(1, 2).bars == bars and bars.tooltip[2][1] == "ptt-radio.hud-route-1")
assert(bars.children[2].value == 1 and bars.children[3].value == 0 and bars.children[1].style.color.g == 0.75)
assert(tail(3, 2).bars.tooltip[1] == "ptt-radio.hud-link-unknown")
handlers.on_player_display_scale_changed({ player_index = 1 })
assert(tail(1, 2).bars.children[3].value == 0)
emit(2, v2(9, gen, 1), 4)
assert(storage.hud.links[1][2] == nil and hud_root(1) == nil)

-- a 0.3.0 save: the gui.left badge goes, the echo panel stays, a saved speaker gets no row
setup()
storage = { speakers = { [1] = 0 }, companions = {}, open_mic = {}, v2 = {}, echo_sources = { [1] = true }, talks = {}, audience_revision = string.rep("\0", 8) }
players[1].gui.left.add({ type = "frame", name = "ptt_radio_badge" })
players[1].gui.left.add({ type = "frame", name = "ptt_radio_echo_panel" })
script.configure({})
assert(players[1].gui.left.ptt_radio_badge == nil and #players[1].gui.left.children == 1)
assert(players[1].gui.left.ptt_radio_echo_panel and hud_root(1) == nil)
game.tick = 31
nth[15]({ tick = 31 })
assert(storage.speakers[1] == nil and hud_root(2) == nil)

-- a 0.3.0 save has no radio tables; existing radio state survives a second configure
setup()
storage = { speakers = {}, companions = {}, v2 = {}, open_mic = {}, echo_sources = {}, talks = {}, audience_revision = string.rep("\0", 8) }
script.configure({})
assert(storage.radio and storage.radio_policy.muted and storage.radio_policy.publishers)
storage.radio[1] = { tx = "item/iron-plate", rx = {}, presets = {} }
storage.radio_policy.muted[2] = true
script.configure({})
assert(storage.radio[1].tx == "item/iron-plate" and storage.radio_policy.muted[2])

-- registered admin mutations reach the audience callback exactly once.
setup()
local revision = storage.audience_revision
commands["radio-admin"]({ parameter = "mute 2" })
assert(storage.radio_policy.muted[2] and storage.audience_revision ~= revision)
revision = storage.audience_revision
commands["radio-admin"]({ parameter = "mute 2" })
assert(storage.audience_revision == revision)

print("control.lua: routing, audience, identity, plaque, hud, admin wiring, bounds and 0.2.0/0.3.0 migration passed")

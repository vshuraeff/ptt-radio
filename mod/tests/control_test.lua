local handlers, nth, sent, sounds, players
local function element(root, name)
  local item = { style = {}, valid = true, name = name }
  item.add = function(spec)
    local child = element(item, spec.name)
    if spec.name then item[spec.name] = child end
    return child
  end
  item.destroy = function() if name then root[name] = nil end end
  return item
end

local function setup()
  handlers, nth, sent, sounds, players = {}, {}, {}, {}, {}
  storage = {}
  settings = { global = { ["ptt-radio-scope"] = { value = "all" } } }
  defines = { events = setmetatable({}, { __index = function(_, key) return key end }) }
  script = {
    on_event = function(event, handler) handlers[event] = handler end,
    on_nth_tick = function(tick, handler) nth[tick] = handler end,
    on_init = function(handler) script.init = handler end,
    on_configuration_changed = function(handler) script.configure = handler end,
  }
  helpers = {
    send_udp = function(port, data, target) sent[#sent + 1] = { port = port, data = data, target = target } end,
    recv_udp = function() end,
  }
  game = {
    tick = 0,
    connected_players = {},
    get_player = function(index) assert(index ~= 0, "player zero is not a LuaPlayer"); return players[index] end,
  }
  for index = 1, 3 do
    players[index] = {
      index = index, connected = true, name = "p" .. index, force = { index = 1 },
      chat_color = { r = 1, g = 0, b = 0 }, gui = { left = element({}, nil) },
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
  dofile(arg[1] or "ptt-radio/mod/ptt-radio/control.lua")
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
assert(sent[1].data == string.char(0x52, 1, 7) .. u16(1) .. u16(42) .. gen .. u16(1))
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

print("control.lua: routing, audience, identity, plaque and bounds passed")

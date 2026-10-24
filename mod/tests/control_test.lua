local control_path = arg[1] or "mod/ptt-radio/control.lua"
package.path = control_path:gsub("[^/]*$", "?.lua") .. ";" .. package.path
local handlers, nth, sent, sounds, players, codec, version_checks
local function element(parent, spec)
  local item = { valid = true, children = {}, parent = parent, style = {} }
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
      if child == item then table.remove(parent.children, i); break end
    end
  end
  return item
end
local function u16(n) return string.char(math.floor(n / 256) % 256, n % 256) end
local function packet(kind, index, body) return string.char(0x52, 1, kind) .. u16(index or 0) .. (body or "") end
local function emit(sender, payload, tick)
  game.tick = tick or game.tick
  handlers.on_udp_packet_received({ player_index = sender, payload = payload, tick = game.tick })
end
local function hello(index, body) emit(index, packet(1, 0, body or "tester hud1 pol1 v2")) end
local function setup(count, cold, saved)
  handlers, nth, sent, sounds, players, version_checks = {}, {}, {}, {}, {}, {}
  storage = saved or {}
  settings = { global = { ["ptt-radio-scope"] = { value = "all" } } }
  prototypes = {
    item = { ["iron-plate"] = {}, ["copper-plate"] = {}, ["parameter"] = { parameter = true } },
    fluid = { water = {} }, virtual_signal = { ["signal-A"] = {} }, entity = {}, recipe = {},
    space_location = { vulcanus = {} }, asteroid_chunk = {}, quality = {},
  }
  defines = { events = setmetatable({}, { __index = function(_, key) return key end }) }
  script = {
    active_mods = { ["ptt-radio"] = "0.4.0" },
    on_event = function(events, handler)
      for _, event in ipairs(type(events) == "table" and events or { events }) do handlers[event] = handler end
    end,
    on_nth_tick = function(tick, handler) nth[tick] = handler end,
    on_init = function(handler) script.init = handler end,
    on_configuration_changed = function(handler) script.configure = handler end,
  }
  commands = { add_command = function(name, _, handler) commands[name] = handler end }
  helpers = { compare_versions = function(first, second)
    version_checks[#version_checks + 1] = { first, second }
    assert(second == "0.4.0")
    return assert(({ ["0.3.9"] = -1, ["0.4.0"] = 0, ["0.10.0"] = 1 })[first])
  end, recv_udp = function() end, send_udp = function(port, data, target)
    sent[#sent + 1] = { port = port, data = data, target = target }
  end }
  game = { tick = 0, players = players, connected_players = {}, print = function() end,
    get_player = function(index) assert(index ~= 0); return players[index] end }
  for index = 1, count or 3 do
    local player = {
      index = index, name = "p" .. index, connected = true, admin = index == 1,
      force = { index = 1 }, physical_surface = { index = 1 }, chat_color = { r = 1, g = 0, b = 0 },
      display_resolution = { width = 1920, height = 1080 }, display_scale = 1,
      gui = { screen = element({ children = {} }), left = element({ children = {} }) },
      mod_settings = { ["ptt-radio-companion-port"] = { value = 34555 + index },
        ["ptt-radio-echo"] = { value = false }, ["ptt-radio-half-duplex"] = { value = true } },
      printed = {}, translations = {}, set_shortcut_toggled = function() end,
    }
    player.print = function(message) player.printed[#player.printed + 1] = message end
    player.request_translation = function(text)
      player.translations[#player.translations + 1] = text
      return #player.translations
    end
    player.play_sound = function(spec) sounds[#sounds + 1] = { index = index, path = spec.path } end
    players[index] = player
    table.insert(game.connected_players, 1, player)
  end
  for _, name in ipairs({ "hud", "channels", "admin", "settings_gui" }) do package.loaded[name] = nil end
  codec = dofile(control_path)
  if not saved then script.init() end
  if not cold then for index = 1, #players do hello(index) end end
  sent, sounds = {}, {}
end
local function clear() sent, sounds = {}, {} end
local function to(index, kind)
  local out = {}
  for _, row in ipairs(sent) do
    if row.target == index and (not kind or row.data:byte(3) == kind) then out[#out + 1] = row.data end
  end
  return out
end
local function policy(index)
  local rows = to(index, 0x15)
  local data = assert(rows[#rows], "missing policy " .. index)
  assert(data:sub(1, 6) == string.char(0x52, 1, 0x15, 0, 0, 1))
  local at = 16
  local function read() local n = data:byte(at) * 256 + data:byte(at + 1); at = at + 2; return n end
  local function list()
    local values, previous = {}, -1
    for _ = 1, read() do
      local n = read(); assert(n > previous); previous = n; values[#values + 1] = n
    end
    assert(#values <= 256)
    return table.concat(values, ",")
  end
  local send_to, receive = list(), list()
  assert(at == #data + 1)
  return send_to, receive, data:byte(15), data:sub(7, 14), data
end
local function expect(index, send_to, receive, flags)
  local a, b, c = policy(index)
  assert(a == send_to, a .. " != " .. send_to)
  assert(b == receive, b .. " != " .. receive)
  assert(c == flags, c .. " != " .. flags)
end
local function gui(index, action, key, state)
  local event = { player_index = index, element = { valid = true,
    name = "ptt-radio-settings-" .. action, tags = { action = action, arg = key }, state = state } }
  if action == "off" or action == "rx" then handlers.on_gui_checked_state_changed(event)
  else handlers.on_gui_click(event) end
end
local function tune(index, key) gui(index, "tune", key) end
local function off(index, value) gui(index, "off", nil, value) end
local GEN, GEN2 = string.char(1, 2, 3, 4, 5, 6, 7, 8), string.char(1, 2, 3, 4, 5, 6, 7, 9)
local function media(kind, revision, generation, talkspurt, opus)
  return packet(kind, 42, (generation or GEN) .. u16(talkspurt or 1) .. (revision or storage.audience_revision) .. (opus or ""))
end
local function root(index) return players[index].gui.screen.ptt_radio_hud end
local function row(index, sender) return root(index) and root(index)["ptt_radio_hud_" .. sender] end
local function stack(index)
  local out = {}
  for _, entry in ipairs(root(index) and root(index).children or {}) do
    out[#out + 1] = entry.name and entry.name:sub(15) or "+" .. entry.content.children[1].caption[2]
  end
  return table.concat(out, ",")
end
local function status(sender, generation, talkspurt, level, route, delay)
  return packet(0x0A, 0, string.char(1, 1) .. u16(sender) .. generation .. u16(talkspurt) .. string.char(level, route or 0, delay or 0))
end
local function level(viewer, sender)
  local links = storage.hud.links[viewer]
  return links and links[sender] and links[sender].level
end
local function find(node, name)
  if node.name == name then return node end
  for _, child in ipairs(node.children) do
    local found = find(child, name)
    if found then return found end
  end
end

-- heartbeats preserve settings drafts, including unrelated capability/version changes.
setup()
handlers["ptt-radio-settings-open"]({ player_index = 1 })
local window = players[1].gui.screen["ptt-radio-settings-window"]
local target, picker = find(window, "ptt-radio-admin-target"), find(window, "ptt-radio-admin-signal")
local kind, mode = find(window, "ptt-radio-admin-kind"), find(window, "ptt-radio-admin-deny-mode")
target.text, picker.elem_value, kind.selected_index, mode.selected_index = "draft player", { type = "item", name = "iron-plate" }, 2, 1
local pane = window.children[2]
pane.vertical_scroll_position = 123
players[1].opened = picker
for _, body in ipairs({ "tester hud1 pol1 v2", "next-version pol1 v2", "next-version hud1 pol1" }) do
  hello(1, body)
  assert(players[1].gui.screen["ptt-radio-settings-window"] == window and window.valid)
  assert(players[1].opened == picker and target.text == "draft player" and picker.elem_value.name == "iron-plate")
  assert(kind.selected_index == 2 and mode.selected_index == 1 and pane.vertical_scroll_position == 123)
end
hello(1, "old hud1 v2")
assert(not window.valid and players[1].gui.screen["ptt-radio-settings-window"] ~= window)

-- an unchanged heartbeat uses cached policy, with zero eligibility work at each size.
for _, count in ipairs({ 16, 32, 64 }) do
  setup(count, true)
  for index = 1, count do storage.companions[index], storage.pol1[index] = 0, true end
  local channels, calls = require("channels"), 0
  local eligible = channels.eligible
  channels.eligible = function(...)
    calls = calls + 1
    return eligible(...)
  end
  handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" })
  assert(calls >= count * count)
  local snapshot, epoch = storage.policy_snapshot, storage.audience_revision
  calls = 0
  clear(); hello(1, "version-changed pol1")
  assert(calls == 0 and storage.policy_snapshot == snapshot and storage.audience_revision == epoch)
  assert(#to(1, 0x15) == 1 and #to(2, 0x15) == 0)
  local _, _, _, _, cached = policy(1)
  clear(); hello(1, "version-next hud1 pol1 v2")
  assert(calls == 0 and select(5, policy(1)) == cached)
  hello(1, "old v2")
  assert(calls >= count * count and storage.audience_revision ~= epoch)
  channels.eligible = eligible
end

-- capabilities are whole whitespace-delimited tokens, independent of order/version.
for _, case in ipairs({
  { "tester hud1 v2 pol1", true, true, true }, { "tester pol1 v2 hud1", true, true, true },
  { "tester\tv2\nhud1\tpol1", true, true, true }, { "tester v2", true, false, false },
  { "tester hud1", false, true, false }, { "tester xv2 v20 hud10 xhud1 pol10 xpol1", false, false, false },
  { "", false, false, false },
}) do
  setup(3, true)
  hello(1, case[1])
  assert((storage.v2[1] == true) == case[2] and (storage.hud1[1] == true) == case[3] and (storage.pol1[1] == true) == case[4])
end

-- cold/old clients receive no policy or audio; signaling remains independent.
setup(3, true)
for _, body in ipairs({ "", "tester v2", "tester pol10 xpol1" }) do
  hello(1, body); assert(not storage.pol1[1] and #to(1, 0x15) == 0)
end
emit(1, packet(6, 3, "opaque\0signal"))
assert(to(3, 0x13)[1] == packet(0x13, 1, "opaque\0signal"))
emit(0, packet(1, 45678))
assert(storage.hub_port == 45678 and not storage.pol1[0])
clear(); emit(1, packet(6, 0, "hub")); assert(sent[1].port == 45678 and sent[1].target == 0)
clear(); emit(0, packet(6, 2, "from hub")); assert(to(2, 0x13)[1] == packet(0x13, 0, "from hub"))
clear(); hello(1); expect(1, "", "", 1)
hello(2); expect(1, "2", "2", 1); expect(2, "1", "1", 1)
assert(#to(3, 0x15) == 0)
clear(); emit(1, media(0x0B)); assert(#to(2, 0x0B) == 1 and #to(3) == 0)
for _, old in ipairs({ packet(2), packet(3, 1, "opus"), packet(4), packet(7, 42, GEN .. u16(1)), packet(8, 42, GEN .. u16(1) .. "opus"), packet(9, 42, GEN .. u16(1)) }) do
  clear(); emit(1, old); emit(0, old); assert(#sent == 0)
end
clear(); emit(0, media(0x0B)); assert(#sent == 0 and not storage.talks[0])
clear(); emit(2, packet(6, 65535, string.rep("x", 1200))); assert(#sent == 4)
for _, entry in ipairs(sent) do assert(#entry.data == 1205 and entry.data:byte(3) == 0x13) end
clear()
for _, bad in ipairs({ "", "R", string.char(0x51, 1, 6, 0, 2), string.char(0x52, 2, 6, 0, 2),
  packet(6, 2, string.rep("x", 1201)), packet(6, 99, "absent"), packet(0x13, 2, "forged") }) do emit(1, bad) end
assert(#sent == 0)
emit(1, packet(6, 3, packet(0x15, 0, "nested")))
assert(to(3, 0x13)[1] == packet(0x13, 1, packet(0x15, 0, "nested")))
clear(); emit(0, packet(1, 100)); emit(0, packet(1, 45678, "body")); assert(#sent == 0 and storage.hub_port == 45678)
emit(0, packet(1, 0)); assert(storage.hub_port == 34555)
local epoch = storage.audience_revision
clear(); hello(1, "next pol1 v2 hud1"); assert(storage.audience_revision == epoch and #to(2, 0x15) == 0)
local talk = storage.talks[1]
hello(1, "old v2"); assert(storage.audience_revision ~= epoch and not storage.speakers[1] and talk.closed)
expect(2, "", "", 1)

-- media capabilities do not hide game HUD/clicks from eligible old/offline viewers.
setup(3, true)
hello(1); hello(2, "tester v2")
clear(); emit(1, media(0x0B))
assert(row(2, 1) and row(3, 1) and #sounds == 2 and #to(2) == 0 and #to(3) == 0)
tune(2, "item/iron-plate"); assert(not row(2, 1) and row(3, 1))
local previous = storage.audience_revision
tune(2, "ptt/any")
clear(); emit(1, media(0x0B, previous))
assert(not row(2, 1) and row(3, 1) and #sounds == 0)
emit(1, media(0x0C, nil, GEN, 1, "opus"))
assert(row(2, 1) and #sounds == 1 and sounds[1].index == 2 and #to(2) == 0)
off(3, true); assert(not row(3, 1))
off(3, false); assert(not row(3, 1))
emit(1, media(0x0C, nil, GEN, 1, "opus")); assert(row(3, 1) and #to(3) == 0)

-- real handlers publish a canonical shared vector and every global revision.
setup(2)
players[1].mod_settings["ptt-radio-echo"].value = true
storage.audience_revision = string.rep("\0", 7) .. "\6"
storage.policy_snapshot, storage.previous_policy = nil, nil
clear(); hello(1)
local _, _, _, _, vector = policy(1)
assert(vector == string.char(0x52, 1, 0x15, 0, 0, 1) .. string.rep("\0", 7) .. "\7" .. string.char(3) .. u16(1) .. u16(2) .. u16(1) .. u16(2))
local old = storage.audience_revision
clear(); tune(1, "item/iron-plate")
assert(storage.audience_revision ~= old and #to(1, 0x15) == 1 and #to(2, 0x15) == 1)
expect(1, "", "", 3); expect(2, "", "", 1)
clear(); commands["radio-admin"]({ parameter = "mute 1" }); expect(1, "", "", 0)
assert(#to(2, 0x15) == 1)
clear(); commands["radio-admin"]({ parameter = "mute 1" }); assert(#sent == 0)
commands["radio-admin"]({ parameter = "unmute 1" })
commands["radio-admin"]({ parameter = "deny 1 item/iron-plate rx" })
expect(1, "", "", 1)
assert(require("channels").tx_permitted(1))
off(1, true); expect(1, "", "", 0)
assert(not require("channels").eligible(0, 1))
clear(); emit(1, media(0x0B)); assert(#sent == 0 and not storage.speakers[1])

-- prior pair eligibility is immutable, including self flags and no future/skipped window.
setup()
local previous = storage.audience_revision
tune(3, "item/iron-plate")
clear(); emit(1, media(0x0B, previous)); assert(row(2, 1) and not row(3, 1) and #to(3) == 0)
assert(#sounds == 1 and sounds[1].index == 2)
local current = storage.audience_revision
clear(); emit(1, media(0x0C, current, GEN, 1, "opus")); assert(#to(2, 0x0C) == 1)
gui(3, "rx", "ptt/any", true)
clear(); emit(1, media(0x0B, current)); assert(not row(3, 1) and #to(3) == 0)
emit(1, media(0x0C, nil, GEN, 1, "opus")); assert(row(3, 1))
local root3 = root(3)
clear(); emit(1, media(0x0D, current)); assert(not row(2, 1) and row(3, 1) and root(3) == root3)
assert(#to(2, 0x0D) == 1 and #to(3) == 0)
emit(1, media(0x0D)); assert(not storage.speakers[1] and not row(3, 1))
setup()
players[1].mod_settings["ptt-radio-echo"].value = true
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-echo", player_index = 1 })
previous = storage.audience_revision
players[1].mod_settings["ptt-radio-echo"].value = false
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-echo", player_index = 1 })
clear(); emit(1, media(0x0B, previous)); assert(#to(1) == 0 and #to(2, 0x0B) == 1)
local state = storage.talks[1]
for _, revision in ipairs({ string.rep("\255", 8), string.rep("\0", 8) }) do
  clear(); emit(1, media(0x0B, revision, GEN2, 2)); assert(#sent == 0 and storage.talks[1] == state)
end
storage.audience_revision = string.rep("\0", 7) .. "\99"
clear(); emit(1, media(0x0C, previous, GEN, 1, "opus")); assert(#sent == 0)

-- revocation discards membership and link state; regrant needs newly accepted media.
setup()
emit(1, media(0x0B)); emit(2, status(1, GEN, 1, 3))
assert(row(2, 1) and level(2, 1) == 3)
off(2, true); assert(not row(2, 1) and not level(2, 1))
off(2, false); assert(not row(2, 1) and not storage.talks[1].visible[2] and row(3, 1))
emit(1, media(0x0C, nil, GEN, 1, "opus")); assert(row(2, 1))

-- validation completes before generation/talkspurt, row or sound mutation.
setup()
for _, bad in ipairs({ "", "R", media(0x0B):sub(1, 22), media(0x0B) .. "x", media(0x0D) .. "x", media(0x0C), media(0x0C, nil, GEN, 1, string.rep("x", 1157)), packet(0x15, 0, "forged") }) do
  clear(); emit(1, bad); assert(#sent == 0 and not storage.talks[1] and #sounds == 0)
end
emit(1, media(0x0C, nil, GEN, 65535, string.rep("x", 1156))); assert(storage.talks[1])
emit(1, media(0x0B, nil, GEN, 0)); assert(storage.talks[1].talkspurt == 0)
local before = storage.talks[1]
for _, bad in ipairs({ media(0x0D, nil, GEN, 65535), media(0x0B, nil, GEN, 65535), media(0x0D, nil, GEN2, 0), media(0x0D, nil, GEN, 1) }) do
  clear(); emit(1, bad); assert(#sent == 0 and storage.talks[1] == before and not before.closed)
end
emit(1, media(0x0B, nil, GEN2, 1)); clear(); emit(1, media(0x0B, nil, GEN, 2)); assert(#sent == 0)
emit(1, media(0x0D, nil, GEN2, 1)); assert(not storage.speakers[1])
clear(); emit(1, media(0x0B, nil, GEN2, 1)); assert(#sent == 0)
setup()
for index = 1, 17 do emit(1, media(0x0B, nil, string.rep("\0", 7) .. string.char(index)), index) end
assert(storage.talks[1].retired_count == 16)
clear(); emit(1, media(0x0B, nil, string.rep("\0", 7) .. "\18"), 18); assert(#sent == 0)
hello(1); game.tick = 3660; nth[60]({ tick = 3660 }); assert(not storage.talks[1])

-- surface follows physical travel, not remote view; force remains the outer boundary.
setup()
tune(1, "ptt/here"); tune(2, "ptt/here")
local epoch = storage.audience_revision
clear(); handlers.on_player_changed_surface({ player_index = 2, surface_index = 90 })
assert(storage.audience_revision == epoch and #sent == 0)
players[2].physical_surface.index = 2
handlers.on_player_changed_surface({ player_index = 2, surface_index = 90 })
expect(1, "", "", 1); expect(2, "", "", 1)
players[2].physical_surface.index = 1; handlers.on_player_changed_surface({ player_index = 2 })
expect(1, "2", "2", 1)
players[2].force.index = 2; settings.global["ptt-radio-scope"].value = "force"
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" }); expect(1, "", "", 1)
settings.global["ptt-radio-scope"].value = "all"
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" }); expect(1, "2", "2", 1)
settings.global["ptt-radio-scope"].value = "force"
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" })
players[2].force.index = 1
handlers.on_player_changed_force({ player_index = 2 }); expect(1, "2", "2", 1)
players[2].force.index = 2
handlers.on_player_changed_force({ player_index = 2 }); expect(1, "", "", 1)
players[2].force.index = 1
handlers.on_forces_merged({}); expect(1, "2", "2", 1)
commands["radio-admin"]({ parameter = "channel fluid/water" })
tune(2, "fluid/water"); expect(2, "", "", 0)
commands["radio-admin"]({ parameter = "publisher add 2" }); expect(2, "1,3", "", 1)
clear(); emit(2, media(0x0B)); assert(row(1, 2) and row(3, 2))
commands["radio-admin"]({ parameter = "mute 2" }); clear(); emit(2, media(0x0C, nil, GEN, 1, "opus")); assert(#sent == 0 and not storage.speakers[2])

-- promotion and demotion immediately publish admin-channel authority and refresh controls.
setup()
commands["radio-admin"]({ parameter = "channel fluid/water" })
tune(2, "fluid/water"); expect(2, "", "", 0)
handlers["ptt-radio-settings-open"]({ player_index = 2 })
local window = players[2].gui.screen["ptt-radio-settings-window"]
assert(not find(window, "ptt-radio-admin-section"))
players[2].admin = true
clear(); handlers.on_player_promoted({ player_index = 2 })
expect(2, "1,3", "", 1)
assert(#to(1, 0x15) == 1 and #to(3, 0x15) == 1 and not window.valid)
window = players[2].gui.screen["ptt-radio-settings-window"]
assert(find(window, "ptt-radio-admin-section"))
emit(2, media(0x0B)); assert(row(1, 2))
players[2].admin = false
clear(); handlers.on_player_demoted({ player_index = 2 })
expect(2, "", "", 0)
expect(1, "3", "3", 1)
assert(not storage.speakers[2] and not row(1, 2) and not window.valid)
assert(not find(players[2].gui.screen["ptt-radio-settings-window"], "ptt-radio-admin-section"))
clear(); emit(2, media(0x0C, nil, GEN, 1, "opus")); assert(#sent == 0)

-- current media changes icons in place; previous media retains its original channel.
setup()
gui(2, "rx", "item/iron-plate", true); gui(3, "rx", "item/iron-plate", true)
local previous = storage.audience_revision
tune(1, "item/iron-plate")
emit(1, media(0x0B, previous)); assert(storage.talks[1].channel == "ptt/any")
emit(2, status(1, GEN, 1, 2, 1, 1))
local old_row, old_root = row(2, 1), root(2)
local icon, bars = old_row.content.children[1], old_row.content.tail.bars
local link = storage.hud.links[2][1]
emit(1, media(0x0C, nil, GEN, 1, "opus"))
assert(row(2, 1) == old_row and root(2) == old_root and icon.sprite == "item/iron-plate" and icon.tooltip == "item/iron-plate")
assert(old_row.content.tail.bars == bars and storage.hud.links[2][1] == link)
local order = stack(2)
emit(1, media(0x0B)); emit(1, media(0x0C, previous, GEN, 1, "opus"))
assert(stack(2) == order and row(2, 1) == old_row and icon.sprite == "item/iron-plate")
local previous = storage.audience_revision
tune(1, "ptt/any")
emit(1, media(0x0C, nil, GEN, 1, "opus")); assert(storage.talks[1].channel == "ptt/any")
emit(1, media(0x0B, previous, GEN, 2)); assert(storage.talks[1].channel == "item/iron-plate")

-- active identity, append/restart order, compaction, reflow and exact quality expiry.
setup()
emit(2, media(0x0B), 1); emit(1, media(0x0B, nil, GEN2), 2)
assert(stack(3) == "2,1")
local old_row = row(3, 2)
emit(2, media(0x0B), 3); assert(row(3, 2) == old_row and stack(3) == "2,1")
local epoch = storage.audience_revision
hello(3); assert(row(3, 2) == old_row and storage.audience_revision == epoch)
emit(2, media(0x0B, nil, GEN, 2), 4); assert(stack(3) == "1,2")
emit(3, status(2, GEN, 2, 3, 1, 1), 10); assert(level(3, 2) == 3)
local bars = row(3, 2).content.tail.bars
emit(3, status(2, GEN, 1, 1), 11); assert(level(3, 2) == 3)
emit(3, status(2, GEN2, 2, 1), 11); assert(level(3, 2) == 3)
local valid = status(2, GEN, 2, 2, 2, 1)
for _, bad in ipairs({ valid:sub(1, -2), valid .. "x", status(2, GEN, 2, 4), status(2, GEN, 2, 2, 4), status(2, GEN, 2, 2, 0, 4), packet(0x0A, 0, string.char(2, 0)), packet(0x0A, 0, string.char(1, 33) .. string.rep(valid:sub(8), 33)), packet(0x0A, 0, string.char(1, 2) .. valid:sub(8) .. status(2, GEN, 2, 4):sub(8)) }) do
  emit(3, bad, 12); assert(level(3, 2) == 3)
end
emit(3, valid, 20); assert(level(3, 2) == 2 and row(3, 2).content.tail.bars == bars)
local maximum = packet(0x0A, 0, string.char(1, 32) .. string.rep(valid:sub(8), 32))
assert(#maximum == 487); emit(3, maximum, 20); assert(level(3, 2) == 2)
emit(0, status(2, GEN, 2, 1), 20); emit(99, status(2, GEN, 2, 1), 20); assert(level(3, 2) == 2)
game.tick = 319; handlers.on_tick({ tick = 319 }); assert(level(3, 2) == 2)
game.tick = 320; handlers.on_tick({ tick = 320 }); assert(not level(3, 2) and row(3, 2).content.tail.bars == bars)
emit(3, valid, 321); emit(2, packet(5, 0, "\1"), 322); assert(not row(3, 2).content.tail.bars)
emit(2, packet(5, 0, "\0"), 323); assert(row(3, 2).content.tail.bars)
players[3].display_resolution = { width = 1280, height = 300 }
handlers.on_player_display_resolution_changed({ player_index = 3 }); assert(root(3).location.x == 8 and root(3).location.y == 120)
emit(2, media(0x0D, nil, GEN, 1), 324); assert(row(3, 2))
emit(2, media(0x0D, nil, GEN, 2), 325); assert(not row(3, 2) and not level(3, 2))
game.tick = 356; nth[15]({ tick = 356 }); assert(not root(3))

-- overflow/reflow keeps earliest activation order; a later restart appends.
setup(4)
players[1].display_resolution = { width = 1280, height = 300 }
for index = 1, 4 do emit(index, media(0x0B), index) end
assert(stack(1) == "1,2,+2" and stack(2) == "1,2,3,4")
players[1].display_resolution = { width = 1920, height = 1080 }
handlers.on_player_display_resolution_changed({ player_index = 1 })
assert(stack(1) == "1,2,3,4" and root(1).location.x == 8 and root(1).location.y == 432)
players[1].display_scale = 2; handlers.on_player_display_scale_changed({ player_index = 1 })
assert(root(1).location.x == 16 and stack(1) == "1,2,3,4")
emit(2, media(0x0D)); assert(stack(3) == "1,3,4")

-- microphone/half-duplex wiring and liveness remain independent of routing policy.
setup()
handlers.on_lua_shortcut({ prototype_name = "ptt-radio-open-mic", player_index = 1 })
assert(storage.open_mic[1] and to(1, 0x11)[1]:byte(6) == 1)
clear(); hello(1); assert(to(1, 0x11)[1]:byte(6) == 1)
players[1].mod_settings["ptt-radio-half-duplex"].value = false
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-half-duplex", player_index = 1 })
assert(to(1, 0x12)[#to(1, 0x12)]:byte(6) == 0)
emit(1, packet(5, 0, "\1")); assert(storage.echo_sources[1] and players[1].gui.left.ptt_radio_echo_panel and not root(2))
emit(1, packet(5, 0, "\2")); assert(storage.echo_sources[1])
game.tick = 900; nth[60]({ tick = 900 }); assert(storage.companions[1] and storage.open_mic[1])
game.tick = 960; nth[60]({ tick = 960 }); assert(not storage.companions[1] and not storage.open_mic[1] and not storage.hud1[1] and not storage.pol1[1] and not storage.echo_sources[1])
clear(); emit(1, packet(6, 0, "expired hub")); assert(#sent == 0)

-- capability loss, heartbeat expiry and malformed late reports never reopen rows.
setup()
emit(2, media(0x0B)); emit(1, status(2, GEN, 1, 3), 10); emit(3, status(2, GEN, 1, 2), 20)
local other = storage.hud.links[3][2]
hello(1, "tester pol1 v2"); assert(not level(1, 2) and storage.hud.links[3][2] == other)
emit(1, status(2, GEN, 1, 3), 21); assert(not level(1, 2))
hello(1); emit(1, status(2, GEN, 1, 3), 30)
game.tick = 269; handlers.on_tick({ tick = 269 }); assert(level(1, 2) == 3)
emit(1, status(2, GEN, 1, 3), 270)
game.tick = 569; handlers.on_tick({ tick = 569 }); assert(level(1, 2) == 3)
game.tick = 570; handlers.on_tick({ tick = 570 }); assert(not level(1, 2))
game.tick = 571; nth[15]({ tick = 571 }); assert(not row(1, 2))
emit(1, status(2, GEN, 1, 3), 572); assert(not level(1, 2) and not row(1, 2))

-- route/delay tooltips cover every enum, including measured zero's grey unknown details.
setup()
emit(2, media(0x0B))
local bars = row(1, 2).content.tail.bars
assert(bars.tooltip[1] == "ptt-radio.hud-link-unknown")
for route = 0, 3 do
  for delay = 0, 3 do
    local measured = (route + delay) % 4
    emit(1, status(2, GEN, 1, measured, route, delay))
    assert(level(1, 2) == measured and row(1, 2).content.tail.bars == bars)
    assert(bars.tooltip[2][1] == "ptt-radio.hud-route-" .. route and bars.tooltip[3][1] == "ptt-radio.hud-delay-" .. delay)
    assert(bars.tooltip[1] == (measured == 0 and "ptt-radio.hud-link-unknown-details" or "ptt-radio.hud-link"))
    if measured == 0 then
      for _, segment in ipairs(bars.children) do
        assert(segment.value == 1 and segment.style.color.r == 0.45 and segment.style.color.g == 0.45 and segment.style.color.b == 0.45)
      end
    end
  end
end
clear(); emit(1, packet(0x0A, 3, status(2, GEN, 1, 3, 1, 1):sub(6)))
assert(level(1, 2) == 3 and not level(3, 2))

-- configure retains dated measurements and expires undated cached measurements.
setup()
emit(2, media(0x0B)); emit(1, status(2, GEN, 1, 3), 2)
local link = storage.hud.links[1][2]
script.configure({})
assert(storage.hud.links[1][2] == link and link.tick == 2 and level(1, 2) == 3)
storage.hud.next_expiry, link.tick = nil, nil
script.configure({}); handlers.on_tick({ tick = game.tick })
assert(not level(1, 2) and row(1, 2).content.tail.bars.tooltip[1] == "ptt-radio.hud-link-unknown")

-- bounds, unsigned carry/overflow rejection, capability loss, leave and save migration.
setup(258, true)
for index = 1, 258 do storage.companions[index], storage.pol1[index] = 0, true end
clear(); hello(1); expect(1, "", "", 0)
setup()
storage.audience_revision = string.rep("\0", 7) .. "\255"
clear(); tune(1, "item/iron-plate"); assert(storage.audience_revision == string.rep("\0", 6) .. "\1\0")
storage.audience_revision = string.rep("\255", 8)
local old_snapshot = storage.policy_snapshot
clear(); local ok, err = pcall(tune, 1, "ptt/any")
assert(not ok and tostring(err):find("revision exhausted", 1, true) and storage.audience_revision == string.rep("\255", 8) and storage.policy_snapshot == old_snapshot and #sent == 0)
setup()
emit(1, media(0x0B)); players[2].connected = false; table.remove(game.connected_players, 2)
handlers.on_player_left_game({ player_index = 2 }); assert(not storage.pol1[2])
players[2].connected = true; game.connected_players[#game.connected_players + 1] = players[2]
handlers.on_player_joined_game({ player_index = 2 }); hello(2)
expect(2, "1,3", "1,3", 1)
game.tick = 960; nth[60]({ tick = 960 }); assert(not storage.pol1[1] and not storage.companions[1])
setup()
storage.radio[1] = { tx = "item/iron-plate", rx = {}, presets = {} }
storage.channel_policy_active = nil
storage.speakers[1], storage.talks[1] = 1, { generation = GEN }
players[1].gui.left.add({ type = "frame", name = "ptt_radio_badge" })
script.configure({}); assert(storage.radio[1].tx == "item/iron-plate" and not storage.speakers[1] and not storage.talks[1] and not players[1].gui.left.ptt_radio_badge)
local epoch = storage.audience_revision
storage.radio[1].tx = "ptt/any"
script.configure({}); assert(storage.audience_revision ~= epoch and storage.policy_snapshot[1].channel == "ptt/any")

-- 0.2.0 bare storage and 0.3.0 radio state migrate without reviving saved talks.
for _, version in ipairs({ "0.2.0", "0.3.0" }) do
  local saved = { speakers = { [1] = 0 }, companions = {}, open_mic = {} }
  if version == "0.3.0" then
    saved.v2, saved.echo_sources, saved.talks = {}, { [1] = true }, { [1] = { generation = GEN } }
    saved.radio = { [1] = { tx = "item/iron-plate", rx = {}, presets = {} } }
    saved.radio_policy = { muted = { [2] = true } }
  end
  setup(3, true, saved)
  players[1].gui.left.add({ type = "frame", name = "ptt_radio_badge" })
  local echo = version == "0.3.0" and players[1].gui.left.add({ type = "frame", name = "ptt_radio_echo_panel" })
  script.configure({ mod_changes = { ["ptt-radio"] = { old_version = version, new_version = "0.4.0" } } })
  assert(storage.channel_policy_active and storage.pol1 and storage.physical_surfaces and storage.policy_snapshot)
  assert(storage.radio_policy.deny and not storage.speakers[1] and not storage.talks[1] and not root(1))
  assert(not players[1].gui.left.ptt_radio_badge)
  if echo then
    assert(players[1].gui.left.ptt_radio_echo_panel == echo and echo.valid and storage.echo_sources[1])
    assert(storage.radio[1].tx == "item/iron-plate" and storage.radio_policy.muted[2])
  end
  hello(1); assert(#to(1, 0x15) >= 1)
end

-- same-version stale saves initialize at their first writable event, without configure.
for _, first in ipairs({ "hello", "surface", "tick", "nth", "admin" }) do
  local saved = { speakers = {}, companions = {}, open_mic = {}, radio = {
    [1] = { tx = "ptt/here", rx = {}, presets = {} }, [2] = { tx = "ptt/here", rx = {}, presets = {} },
  }, radio_policy = { muted = { [3] = true } } }
  setup(3, true, saved)
  assert(storage == saved and storage.pol1 == nil and storage.channel_policy_active == nil)
  if first == "hello" then hello(1)
  elseif first == "surface" then
    players[2].physical_surface.index = 2
    handlers.on_player_changed_surface({ player_index = 2 })
  elseif first == "tick" then handlers.on_tick({ tick = 0 })
  elseif first == "admin" then
    commands["radio-admin"]({ parameter = "mute 2" })
    assert(storage.radio_policy.muted[2])
  else nth[15]({ tick = 0 }) end
  assert(storage.pol1 and storage.physical_surfaces[2] == players[2].physical_surface.index and storage.policy_snapshot)
  assert(storage.radio[1].tx == "ptt/here" and storage.radio_policy.muted[3] and storage.channel_policy_active)
  local snapshot, surfaces = storage.policy_snapshot, storage.physical_surfaces
  hello(1); hello(2)
  snapshot = storage.policy_snapshot
  hello(1); handlers.on_tick({ tick = 1 })
  assert(storage.policy_snapshot == snapshot and storage.physical_surfaces == surfaces)
end

setup(3, true, { speakers = {}, companions = {}, open_mic = {} })
commands["radio-admin"]({ parameter = "mute 2" })
assert(storage.channel_policy_active and storage.radio_policy.muted[2] and storage.policy_snapshot)
local snapshot = storage.policy_snapshot
commands["radio-admin"]({ parameter = "mute 2" })
assert(storage.policy_snapshot == snapshot)

-- self-test codec: SELF_TEST is 7 bytes, SELF_TEST_RESULT exactly 16, all big-endian
do
  setup(1)
  assert(codec.encode_self_test(0x1234) == string.char(0x52, 1, 0x16, 0, 0, 0x12, 0x34), "SELF_TEST layout")
  assert(#codec.encode_self_test(65535) == 7, "SELF_TEST length")
  local function result(overrides)
    local f = { id = 0xabcd, outcome = 0, tone = 1, peak = -20, sent = 150, returned = 148, ms = 90 }
    for key, value in pairs(overrides or {}) do f[key] = value end
    return string.char(0x52, 1, 0x18, 0, 0) .. u16(f.id) .. string.char(f.outcome, f.tone, f.peak % 256)
      .. u16(f.sent) .. u16(f.returned) .. u16(f.ms)
  end
  local ok = codec.parse_self_test_result(result())
  assert(ok and ok.test_id == 0xabcd and ok.outcome == 0 and ok.tone == 1 and ok.mic_peak_dbfs == -20
    and ok.frames_sent == 150 and ok.frames_returned == 148 and ok.first_return_ms == 90, "RESULT fields")
  assert(#result() == 16)
  assert(codec.parse_self_test_result(result({ peak = -128 })).mic_peak_dbfs == -128, "no capture")
  assert(codec.parse_self_test_result(result({ peak = -127 })).mic_peak_dbfs == -127)
  assert(codec.parse_self_test_result(result({ peak = 0 })).mic_peak_dbfs == 0)
  assert(codec.parse_self_test_result(result({ outcome = 4, sent = 65535, ms = 65535 })).first_return_ms == 65535)
  assert(codec.parse_self_test_result(result({ outcome = 5 })) == nil, "unknown outcome")
  assert(codec.parse_self_test_result(result():sub(1, 15)) == nil, "short")
  assert(codec.parse_self_test_result(result() .. "\0") == nil, "long")
  assert(codec.parse_self_test_result("") == nil)
  assert(codec.parse_self_test_result("\x51" .. result():sub(2)) == nil, "magic")
  assert(codec.parse_self_test_result(result():sub(1, 1) .. "\2" .. result():sub(3)) == nil, "version")
  assert(codec.parse_self_test_result(result():sub(1, 2) .. "\x16" .. result():sub(4)) == nil, "kind")
  -- the reserved header field is ignored
  assert(codec.parse_self_test_result(result():sub(1, 3) .. "\0\7" .. result():sub(6)), "reserved field")
end

-- installation help opens at the join deadline, once per session, unless HELLO arrives.
local WINDOW = "ptt-radio-settings-window"
local function tick(at)
  game.tick = at
  handlers.on_tick({ tick = at })
end
setup(2, true)
game.tick = 100
handlers.on_player_joined_game({ player_index = 1 })
hello(2)
tick(699)
assert(not players[1].gui.screen[WINDOW] and not players[2].gui.screen[WINDOW])
tick(700)
assert(players[1].gui.screen[WINDOW] and players[1].opened == players[1].gui.screen[WINDOW])
assert(players[1].printed[1][1] == "ptt-radio-settings.companion-notice")
assert(not players[2].gui.screen[WINDOW])
gui(1, "close"); tick(900)
assert(not players[1].gui.screen[WINDOW] and #players[1].printed == 1)
handlers["ptt-radio-toggle-mic"]({ player_index = 1 })
assert(players[1].gui.screen[WINDOW] and not storage.open_mic[1])
gui(1, "close")
players[1].connected = false
handlers.on_player_left_game({ player_index = 1 })
players[1].connected = true
handlers.on_player_joined_game({ player_index = 1 })
tick(1499); assert(not players[1].gui.screen[WINDOW])
tick(1500); assert(players[1].gui.screen[WINDOW])
setup(1, true)
tick(599); assert(not players[1].gui.screen[WINDOW])
hello(1); tick(600); assert(not players[1].gui.screen[WINDOW])

-- the parsed HELLO version reaches the engine comparison helper; heartbeats keep drafts.
setup(1)
handlers["ptt-radio-settings-open"]({ player_index = 1 })
window = players[1].gui.screen[WINDOW]
assert(find(window, "ptt-radio-settings-companion-status").caption[1] == "ptt-radio-settings.companion-connected-unknown")
target = find(window, "ptt-radio-admin-target")
target.text = "unfinished admin draft"
for _, case in ipairs({ { "0.3.9", "older" }, { "0.4.0", "matching" }, { "0.10.0", "newer" } }) do
  hello(1, "ptt-radio-companion " .. case[1] .. " hud1 pol1 v2")
  local caption = find(window, "ptt-radio-settings-companion-status").caption
  assert(caption[1] == "ptt-radio-settings.companion-connected" and caption[2] == case[1] and caption[3] == "0.4.0")
  assert(caption[4][1] == "ptt-radio-settings.companion-" .. case[2])
  assert(version_checks[#version_checks][1] == case[1])
  assert(players[1].gui.screen[WINDOW] == window and target.text == "unfinished admin draft")
end
for _, invalid in ipairs({ "tester 0.4.0", "ptt-radio-companion invalid", "ptt-radio-companion 0.4.0oops", "ptt-radio-companion 999999999999999999999.0.0" }) do
  local count = #version_checks
  hello(1, invalid .. " hud1 pol1 v2")
  assert(not storage.companion_versions[1] and #version_checks == count)
  assert(find(window, "ptt-radio-settings-companion-status").caption[1] == "ptt-radio-settings.companion-connected-unknown")
end
for _, key in ipairs({ "install-macos", "install-linux", "install-windows", "steam-macos", "steam-linux", "steam-windows", "repository", "wiki" }) do
  local box = find(window, "ptt-radio-settings-" .. key)
  assert(box.type == "text-box" and box.read_only and box.selectable and box.enabled ~= false)
  local id
  for request, element in pairs(storage.settings_gui.translations[1]) do if element == box then id = request end end
  assert(id and players[1].translations[id][1] == "ptt-radio-settings.companion-" .. key .. "-text")
  handlers.on_string_translated({ player_index = 2, id = id, translated = true, result = "wrong player" })
  assert(not box.text)
  handlers.on_string_translated({ player_index = 1, id = id, translated = true, result = "copyable " .. key })
  assert(box.text == "copyable " .. key and not storage.settings_gui.translations[1][id])
end
local old_id = #players[1].translations
gui(1, "close")
handlers.on_string_translated({ player_index = 1, id = old_id, translated = true, result = "late" })
handlers["ptt-radio-settings-open"]({ player_index = 1 })
window = players[1].gui.screen[WINDOW]
local box = find(window, "ptt-radio-settings-install-macos")
handlers.on_string_translated({ player_index = 1, id = old_id - 7, translated = true, result = "stale" })
assert(not box.text)
game.tick = 901; nth[60]({ tick = 901 })
assert(find(window, "ptt-radio-settings-companion-status").caption[1] == "ptt-radio-settings.companion-offline")

local function test_result(id, outcome)
  return packet(0x18, 65535, u16(id) .. string.char(outcome or 0, 1, 236) .. u16(150) .. u16(148) .. u16(90))
end
-- only the requester gets SELF_TEST; the grant and revoke use ordinary POLICY epochs.
setup(2)
local before = storage.audience_revision
handlers["ptt-radio-settings-open"]({ player_index = 1 })
window = players[1].gui.screen[WINDOW]
target = find(window, "ptt-radio-admin-target")
target.text = "pending test draft"
gui(1, "self-test")
local pending = assert(storage.self_tests[1])
assert(find(window, "ptt-radio-settings-test-status").caption[1] == "ptt-radio-settings.test-pending")
assert(players[1].gui.screen[WINDOW] == window and target.text == "pending test draft")
assert(pending.deadline == 900 and not storage.self_tests[2])
assert(storage.audience_revision ~= before)
expect(1, "2", "2", 3); expect(2, "1", "1", 1)
assert(to(1, 0x16)[1] == packet(0x16, 0, u16(pending.id)) and #to(1, 0x16) == 1 and #to(2, 0x16) == 0)
assert(sent[#sent].data == to(1, 0x16)[1] and sent[#sent].target == 1 and sent[#sent].port == 34556)
local granted = storage.audience_revision
clear(); gui(1, "self-test")
assert(#sent == 0 and storage.self_tests[1] == pending and storage.audience_revision == granted)
assert(storage.companion_test_status[1][1] == "ptt-radio-settings.test-running")
emit(2, test_result(pending.id)); emit(0, test_result(pending.id))
emit(1, test_result((pending.id + 1) % 65536))
emit(1, test_result(pending.id):sub(1, 15)); emit(1, test_result(pending.id) .. "\0")
assert(storage.self_tests[1] == pending and storage.audience_revision == granted and #sent == 0)
emit(1, media(0x0B)); emit(1, media(0x0C, nil, nil, nil, "test voice"))
assert(#to(1, 0x0C) == 1)
clear(); emit(1, test_result(pending.id))
assert(not storage.self_tests[1] and storage.audience_revision ~= granted)
expect(1, "2", "2", 1)
local result = storage.companion_test_status[1]
assert(find(window, "ptt-radio-settings-test-status").caption == result)
assert(result[1] == "ptt-radio-settings.test-result" and result[2][1] == "ptt-radio-settings.test-outcome-0")
assert(result[3][1] == "ptt-radio-settings.test-tone-yes" and result[4] == -20 and result[5] == 150 and result[6] == 148 and result[7] == 90)
clear(); emit(1, media(0x0C, granted, nil, nil, "late test voice"))
assert(#to(1, 0x0C) == 0, "previous revision cannot retain self-monitor")
emit(1, test_result(pending.id)); assert(storage.companion_test_status[1] == result)

-- simultaneous tests stay isolated, and every result outcome revokes the temporary grant.
for outcome = 0, 4 do
  setup(2)
  gui(1, "self-test"); gui(2, "self-test")
  local one, two = storage.self_tests[1], storage.self_tests[2]
  assert(one.id ~= two.id)
  emit(2, test_result(one.id)); assert(storage.self_tests[1] == one and storage.self_tests[2] == two)
  emit(1, test_result(one.id, outcome))
  assert(not storage.self_tests[1] and storage.self_tests[2] == two)
  assert(not storage.policy_snapshot[1].self and storage.policy_snapshot[2].self)
  assert(storage.companion_test_status[1][2][1] == "ptt-radio-settings.test-outcome-" .. outcome)
  emit(2, test_result(two.id, outcome)); assert(not storage.policy_snapshot[2].self)
end
setup(1)
gui(1, "self-test")
emit(1, packet(0x18, 0, u16(storage.self_tests[1].id) .. string.char(2, 0, 128) .. u16(0) .. u16(0) .. u16(0)))
assert(storage.companion_test_status[1][3][1] == "ptt-radio-settings.test-tone-no")
assert(storage.companion_test_status[1][4][1] == "ptt-radio-settings.test-no-capture")

-- timeout is tick-based and exact; the persistent echo preference is never overwritten.
setup(1)
gui(1, "self-test"); pending = storage.self_tests[1]
tick(899); assert(storage.self_tests[1] == pending and storage.policy_snapshot[1].self)
tick(900); assert(not storage.self_tests[1] and not storage.policy_snapshot[1].self)
assert(storage.companion_test_status[1][1] == "ptt-radio-settings.test-timeout")
emit(1, test_result(pending.id)); assert(storage.companion_test_status[1][1] == "ptt-radio-settings.test-timeout")
for _, echo in ipairs({ false, true }) do
  setup(1)
  players[1].mod_settings["ptt-radio-echo"].value = echo
  handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-echo", player_index = 1 })
  gui(1, "self-test"); pending = storage.self_tests[1]
  emit(1, test_result(pending.id))
  assert(storage.policy_snapshot[1].self == echo and players[1].mod_settings["ptt-radio-echo"].value == echo)
end
for _, completion in ipairs({ "result", "timeout" }) do
  setup(1)
  game.tick = 100
  gui(1, "self-test")
  pending = storage.self_tests[1]
  local saved, epoch = storage, storage.audience_revision
  setup(1, true, saved)
  assert(storage.self_tests[1] == pending and storage.policy_snapshot[1].self and storage.audience_revision == epoch)
  tick(999); assert(storage.self_tests[1] == pending)
  if completion == "result" then emit(1, test_result(pending.id)) else tick(1000) end
  assert(not storage.self_tests[1] and not storage.policy_snapshot[1].self and storage.audience_revision ~= epoch)
  gui(1, "self-test")
  local next_test = storage.self_tests[1]
  assert(next_test.id ~= pending.id)
  emit(1, test_result(pending.id)); assert(storage.self_tests[1] == next_test)
end
setup(1)
off(1, true); gui(1, "self-test")
assert(storage.self_tests[1] and not storage.policy_snapshot[1].tx and not storage.policy_snapshot[1].self)
emit(1, test_result(storage.self_tests[1].id, 1)); assert(not storage.self_tests[1])
setup(1)
storage.radio_policy.muted[1] = true
handlers.on_runtime_mod_setting_changed({ setting = "ptt-radio-scope" })
gui(1, "self-test"); assert(not storage.policy_snapshot[1].self and not storage.policy_snapshot[1].tx)

-- disconnect, capability loss and rejoin clear the grant; stale IDs cannot end a new test.
for _, reason in ipairs({ "timeout", "left", "capability", "rejoin" }) do
  setup(2)
  gui(1, "self-test"); pending = storage.self_tests[1]
  if reason == "timeout" then
    game.tick = 901; nth[60]({ tick = 901 })
  elseif reason == "left" then
    players[1].connected = false
    table.remove(game.connected_players, 2)
    handlers.on_player_left_game({ player_index = 1 })
  elseif reason == "capability" then hello(1, "old hud1 v2")
  else handlers.on_player_joined_game({ player_index = 1 }) end
  assert(not storage.self_tests[1] and not (storage.policy_snapshot[1] and storage.policy_snapshot[1].self))
  players[1].connected = true
  hello(1)
  clear(); gui(1, "self-test")
  local next_test = assert(storage.self_tests[1])
  assert(next_test.id ~= pending.id)
  emit(1, test_result(pending.id)); assert(storage.self_tests[1] == next_test)
end
setup(1, true)
gui(1, "self-test"); assert(not storage.self_tests[1] and #to(1, 0x16) == 0)
hello(1, "old hud1 v2"); clear()
gui(1, "self-test"); assert(not storage.self_tests[1] and #sent == 0)

-- exported bytes are generated by the real producer after real settings handlers.
if arg[2] == "--groups" then
  setup(4)
  tune(1, "item/iron-plate"); tune(2, "item/iron-plate")
  tune(3, "item/copper-plate"); tune(4, "item/copper-plate")
  clear()
  for index = 1, 4 do hello(index) end
  for index = 1, 4 do
    local _, _, _, _, data = policy(index)
    local hex = data:gsub(".", function(byte) return string.format("%02x", byte:byte()) end)
    print("group-policy " .. index .. " " .. hex)
  end
end
print("control.lua: live channel policy, window, wire, hud, quality, events and migration passed")

-- run from ptt-radio/: lua mod/tests/settings_gui_test.lua [mod/ptt-radio/control.lua]
local control_path = arg[1] or "mod/ptt-radio/control.lua"
local dir = (control_path:gsub("[^/]*$", ""))
package.path = dir .. "?.lua;" .. package.path
local channels = require("channels")
local gui = require("settings_gui")

local ANY, HERE = channels.ANY, channels.HERE
local IRON, COPPER, WATER = "item/iron-plate", "item/copper-plate", "fluid/water"
local PREFIX = "ptt-radio-settings-"
local WINDOW, STATUS, ADMIN = PREFIX .. "window", PREFIX .. "status", PREFIX .. "admin"

-- a gui element keeps its add spec, its children in order and named children as fields
local function element(parent, spec)
  local item = { valid = true, children = {}, parent = parent }
  for key, value in pairs(spec or {}) do item[key] = value end
  item.style = {}
  item.clear = function()
    while #item.children > 0 do item.children[1].destroy() end
  end
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

local players, sent, handlers
local function setup()
  players, sent, handlers = {}, {}, {}
  storage = {}
  settings = { global = { ["ptt-radio-scope"] = { value = "all" } } }
  prototypes = {
    item = { ["iron-plate"] = {}, ["copper-plate"] = {}, ["item-parameter-0"] = { parameter = true } },
    fluid = { water = {} },
    virtual_signal = {
      ["signal-A"] = {}, ["signal-each"] = {}, ["signal-anything"] = {}, ["signal-everything"] = {},
    },
    entity = { ["oil-refinery"] = {} },
    recipe = {}, space_location = { vulcanus = {} }, asteroid_chunk = {}, quality = { rare = {} },
  }
  game = {
    tick = 0,
    connected_players = {},
    players = players,
    get_player = function(index) assert(index ~= 0, "player zero is not a LuaPlayer"); return players[index] end,
  }
  for index = 1, 3 do
    local player = {
      index = index, connected = true, admin = index == 1, name = "p" .. index,
      force = { index = 1 }, physical_surface = { index = 1 }, chat_color = { r = 1, g = 0, b = 0 },
      gui = { left = element({ children = {} }), screen = element({ children = {} }) },
      display_resolution = { width = 1920, height = 1080 }, display_scale = 1,
      mod_settings = {
        ["ptt-radio-companion-port"] = { value = 34555 + index },
        ["ptt-radio-half-duplex"] = { value = true },
        ["ptt-radio-echo"] = { value = false },
      },
      printed = {}, set_shortcut_toggled = function() end, play_sound = function() end,
    }
    player.print = function(message) player.printed[#player.printed + 1] = message end
    players[index] = player
    game.connected_players[index] = player
  end
  channels.init()
  gui.init()
end

local function win(index) return players[index or 1].gui.screen[WINDOW] end
local function find(node, name)
  for _, child in ipairs(node.children) do
    if child.name == name then return child end
    local found = find(child, name)
    if found then return found end
  end
end
local function find_caption(node, key)
  for _, child in ipairs(node.children) do
    if type(child.caption) == "table" and child.caption[1] == key then return child end
    local found = find_caption(child, key)
    if found then return found end
  end
end
local function enabled(el) return el.enabled ~= false end
local function key_of(loc) return loc[1]:sub(#"ptt-radio-settings." + 1) end

-- every localised key the built windows and printed lines use, checked against the cfg files
local used = {}
local function scan(value)
  if type(value) ~= "table" then return end
  if type(value[1]) == "string" and value[1]:sub(1, 19) == "ptt-radio-settings." then used[key_of(value)] = true end
  for _, part in ipairs(value) do scan(part) end
end
local function audit(node)
  scan(node.caption)
  scan(node.tooltip)
  for _, child in ipairs(node.children) do audit(child) end
end
local function audit_all()
  for _, player in ipairs(players) do
    audit(player.gui.screen)
    for _, line in ipairs(player.printed) do scan(line) end
  end
end

local function fire(kind, name, mutate, index)
  index = index or 1
  local el = find(win(index), name)
  assert(el, "no element " .. name)
  if mutate then mutate(el) end
  local handler = ({
    click = gui.on_gui_click, elem = gui.on_gui_elem_changed, checked = gui.on_gui_checked_state_changed,
  })[kind]
  assert(handler({ element = el, player_index = index }) == true, name .. " was not claimed")
  audit_all()
end
local function status(index)
  local el = win(index or 1) and find(win(index or 1), STATUS)
  return el and key_of(el.caption[4])
end
local function presets(index) return table.concat(channels.state(index or 1).presets, ",") end
local function ok(changed, reason) assert(changed == true, tostring(reason)) end

local changes
local function fresh()
  setup()
  changes = 0
  gui.configure(function() changes = changes + 1 end)
end

-- open, close, foreign elements
fresh()
gui.open(players[1])
assert(win() and players[1].opened == win() and #players[1].gui.screen.children == 1)
gui.open(players[1])
assert(#players[1].gui.screen.children == 1 and players[1].opened == win())
gui.toggle(players[1])
assert(win() == nil)
gui.toggle(players[1])
assert(win() ~= nil)
local stray = element(players[1].gui.screen, { name = "elsewhere" })
assert(gui.on_gui_closed({ element = stray, player_index = 1 }) == false)
assert(gui.on_gui_closed({ player_index = 1 }) == false)
assert(gui.on_gui_closed({ element = win(), player_index = 1 }) == true and win() == nil)
gui.refresh(players[1])
assert(win() == nil, "refresh must not open a closed window")
gui.open(players[1])
fire("click", PREFIX .. "close")
assert(win() == nil)
gui.open(players[1])
local dead = find(win(), PREFIX .. "close")
for _, foreign in ipairs({
  { element = stray, player_index = 1 }, { element = element(nil, { name = "ptt-radio-admin-x" }), player_index = 1 },
  { element = element(nil, {}), player_index = 1 }, { player_index = 1 },
}) do
  assert(gui.on_gui_click(foreign) == false and gui.on_gui_elem_changed(foreign) == false)
  assert(gui.on_gui_checked_state_changed(foreign) == false)
end
dead.valid = false
assert(gui.on_gui_click({ element = dead, player_index = 1 }) == false)
assert(win() and changes == 0)
audit_all()

-- tx: a valid pick, a rejected special signal, a lock, the ANY and HERE buttons
fresh()
gui.open(players[1])
local pick = PREFIX .. "tx-pick"
assert(find(win(), pick).elem_value == nil and find(win(), pick).elem_type == "signal")
fire("elem", pick, function(el) el.elem_value = { type = "item", name = "iron-plate" } end)
assert(channels.tx(1) == IRON and changes == 1 and status() == nil)
assert(find(win(), pick).elem_value.name == "iron-plate" and find(win(), pick).elem_value.type == "item")
fire("elem", pick, function(el) el.elem_value = { type = "virtual", name = "signal-each" } end)
assert(channels.tx(1) == IRON and changes == 1 and status() == "refused-unusable-signal")
assert(find(win(), pick).elem_value.name == "iron-plate", "the old value is kept")
fire("elem", pick, function(el) el.elem_value = nil end)
assert(channels.tx(1) == IRON and status() == nil and find(win(), pick).elem_value.name == "iron-plate")
fire("elem", pick, function(el) el.elem_value = { type = "item", name = "item-parameter-0" } end)
assert(channels.tx(1) == IRON and status() == "refused-unusable-signal")
storage.radio_policy.lock[1] = "tx"
fire("elem", pick, function(el) el.elem_value = { type = "item", name = "copper-plate" } end)
assert(channels.tx(1) == IRON and changes == 1 and status() == "refused-locked")
gui.refresh(players[1])
assert(not enabled(find(win(), pick)) and key_of(find(win(), pick).tooltip) == "lock-tx")
assert(not enabled(find(win(), PREFIX .. "tune-" .. ANY)) and not enabled(find(win(), PREFIX .. "tune-" .. HERE)))
storage.radio_policy.lock[1] = nil
gui.refresh(players[1])
assert(enabled(find(win(), pick)) and enabled(find(win(), PREFIX .. "tune-" .. HERE)))
fire("click", PREFIX .. "tune-" .. HERE)
assert(channels.tx(1) == HERE and changes == 2 and find(win(), PREFIX .. "tune-" .. HERE).toggled == true)
assert(find(win(), pick).elem_value == nil)
fire("click", PREFIX .. "tune-" .. ANY)
assert(channels.tx(1) == ANY and changes == 3)
fire("click", PREFIX .. "tune-" .. ANY)
assert(changes == 3 and status() == nil, "a no-op is not a change")
ok(channels.policy_confine(1, 1, { ANY }))
gui.refresh(players[1])
assert(not enabled(find(win(), PREFIX .. "tune-" .. HERE)) and key_of(find(win(), PREFIX .. "tune-" .. HERE).tooltip) == "refused-confined")
fire("click", PREFIX .. "tune-" .. HERE)
assert(channels.tx(1) == ANY and status() == "refused-confined")
ok(channels.policy_confine(1, 1, nil))
ok(channels.policy_ban(1, HERE, true))
gui.refresh(players[1])
assert(not enabled(find(win(), PREFIX .. "tune-" .. HERE)) and key_of(find(win(), PREFIX .. "tune-" .. HERE).tooltip) == "refused-banned")
ok(channels.policy_ban(1, HERE, false))
ok(channels.policy_deny(1, 1, HERE, "tx"))
gui.refresh(players[1])
assert(key_of(find(win(), PREFIX .. "tune-" .. HERE).tooltip) == "refused-denied")
audit_all()

-- tx section notes: muted, and a listener-only sender on the admin channel
fresh()
gui.open(players[2])
assert(find_caption(win(2), "ptt-radio-settings.tx-muted") == nil)
ok(channels.policy_mute(1, 2, true))
gui.refresh(players[2])
assert(find_caption(win(2), "ptt-radio-settings.tx-muted"), "a muted player sees the note")
ok(channels.policy_mute(1, 2, nil))
ok(channels.policy_admin_channel(1, COPPER))
gui.refresh(players[2])
assert(find_caption(win(2), "ptt-radio-settings.tx-muted") == nil)
assert(find_caption(win(2), "ptt-radio-settings.tx-not-publisher") == nil)
ok(channels.set_tx(2, COPPER))
gui.refresh(players[2])
assert(find_caption(win(2), "ptt-radio-settings.tx-not-publisher"))
ok(channels.policy_publisher(1, 2, true))
gui.refresh(players[2])
assert(find_caption(win(2), "ptt-radio-settings.tx-not-publisher") == nil)
audit_all()

-- rx: toggle, the tx channel, a lock, the admin channel
fresh()
ok(channels.set_tx(2, COPPER))
ok(channels.set_tx(3, WATER))
gui.open(players[1])
local copper_rx = PREFIX .. "rx-" .. COPPER
assert(find(win(), copper_rx).state == false and enabled(find(win(), copper_rx)))
fire("checked", copper_rx, function(el) el.state = true end)
assert(channels.state(1).rx[1] == COPPER and changes == 1 and find(win(), copper_rx).state == true)
fire("checked", copper_rx, function(el) el.state = false end)
assert(#channels.state(1).rx == 0 and changes == 2 and find(win(), copper_rx).state == false)
local tx_box = find(win(), PREFIX .. "rx-" .. ANY)
assert(tx_box.state == true and not enabled(tx_box) and key_of(tx_box.tooltip) == "rx-tx-always")
storage.radio_policy.lock[1] = "rx"
fire("checked", copper_rx, function(el) el.state = true end)
assert(#channels.state(1).rx == 0 and status() == "refused-locked")
assert(not enabled(find(win(), copper_rx)) and key_of(find(win(), copper_rx).tooltip) == "lock-rx")
storage.radio_policy.lock[1] = nil
ok(channels.policy_ban(1, WATER, true))
gui.refresh(players[1])
assert(find(win(), PREFIX .. "rx-" .. WATER) == nil, "a banned channel nobody can use is not listed")
ok(channels.policy_ban(1, WATER, false))
ok(channels.policy_deny(1, 1, WATER, "rx"))
gui.refresh(players[1])
local water_rx = find(win(), PREFIX .. "rx-" .. WATER)
assert(not enabled(water_rx) and key_of(water_rx.tooltip) == "refused-denied")
ok(channels.policy_deny(1, 1, WATER, nil))
ok(channels.policy_admin_channel(1, COPPER))
gui.refresh(players[1])
local admin_box = find(win(), copper_rx)
assert(admin_box.state == true and not enabled(admin_box) and key_of(admin_box.tooltip) == "rx-admin")
assert(find_caption(win(), "ptt-radio-settings.known-admin"), "the known list marks the admin channel")
fire("checked", copper_rx, function(el) el.state = false end)
assert(status() == "refused-mandatory" and find(win(), copper_rx).state == true)
ok(channels.set_off(1, true))
gui.refresh(players[1])
assert(find(win(), copper_rx).state == true and not enabled(find(win(), copper_rx)),
  "the admin channel subscription stays locked while off, matching channels.set_rx")
audit_all()

-- presets: add, reorder, remove, limits, cycle
fresh()
gui.open(players[1])
local add = PREFIX .. "preset-add"
fire("click", add)
assert(presets() == ANY and changes == 1 and not enabled(find(win(), add)))
fire("click", add)
assert(presets() == ANY and status() == "refused-duplicate" and changes == 1)
ok(channels.set_tx(1, IRON))
gui.refresh(players[1])
fire("click", add)
ok(channels.set_tx(1, COPPER))
gui.refresh(players[1])
fire("click", add)
assert(presets() == ANY .. "," .. IRON .. "," .. COPPER)
assert(not enabled(find(win(), PREFIX .. "preset-up-1")) and not enabled(find(win(), PREFIX .. "preset-down-3")))
fire("click", PREFIX .. "preset-up-3")
assert(presets() == ANY .. "," .. COPPER .. "," .. IRON)
fire("click", PREFIX .. "preset-down-1")
assert(presets() == COPPER .. "," .. ANY .. "," .. IRON)
fire("click", PREFIX .. "preset-remove-2")
assert(presets() == COPPER .. "," .. IRON and changes == 6)
-- a stale index changes nothing
local before = changes
local stale = element(nil, { name = PREFIX .. "preset-remove-9", tags = { action = "preset-remove", arg = 9 } })
assert(gui.on_gui_click({ element = stale, player_index = 1 }) == true and changes == before and presets() == COPPER .. "," .. IRON)
-- cycle: the button, then the hotkey path with and without a window
fire("click", PREFIX .. "cycle")
assert(channels.tx(1) == IRON)
gui.cycle(players[1])
assert(channels.tx(1) == COPPER and #players[1].printed == 0)
gui.close(players[1])
gui.cycle(players[1])
assert(channels.tx(1) == IRON and key_of(players[1].printed[1][2]) == "cycled")
storage.radio_policy.lock[1] = "tx"
gui.cycle(players[1])
assert(channels.tx(1) == IRON and key_of(players[1].printed[2][4]) == "refused-locked")
gui.open(players[1])
assert(not enabled(find(win(), PREFIX .. "cycle")) and key_of(find(win(), PREFIX .. "cycle").tooltip) == "lock-tx")
storage.radio_policy.lock[1] = "all"
gui.refresh(players[1])
assert(not enabled(find(win(), add)) and not enabled(find(win(), PREFIX .. "preset-remove-1")))
fire("click", PREFIX .. "preset-remove-1")
assert(presets() == COPPER .. "," .. IRON and status() == "refused-locked")
storage.radio_policy.lock[1] = nil
-- the eighth preset fills the list
ok(channels.set_presets(1, { ANY, HERE, COPPER, WATER, "virtual/signal-A", "space-location/vulcanus", "entity/oil-refinery" }))
assert(channels.tx(1) == IRON)
gui.refresh(players[1])
assert(enabled(find(win(), add)))
fire("click", add)
assert(#channels.state(1).presets == 8)
assert(not enabled(find(win(), add)) and key_of(find(win(), add).tooltip) == "preset-full")
fire("click", add)
assert(#channels.state(1).presets == 8 and status() == "refused-too-many")
ok(channels.set_presets(1, {}))
gui.refresh(players[1])
assert(not enabled(find(win(), PREFIX .. "cycle")) and key_of(find(win(), PREFIX .. "cycle").tooltip) == "refused-no-presets")
fire("click", PREFIX .. "cycle")
assert(status() == "refused-no-presets")
audit_all()

-- the off switch is never disabled and works under every lock
fresh()
ok(channels.policy_lock(1, 1, "all"))
gui.open(players[1])
local off = PREFIX .. "off"
assert(enabled(find(win(), off)) and find(win(), off).state == false)
assert(key_of(find(win(), off).tooltip) == "off-tooltip" and key_of(find(win(), off).caption) == "off-caption")
assert(not enabled(find(win(), PREFIX .. "tx-pick")) and not enabled(find(win(), PREFIX .. "known-" .. ANY)))
assert(not enabled(find(win(), PREFIX .. "preset-add")) and not enabled(find(win(), PREFIX .. "tune-" .. HERE)))
fire("checked", off, function(el) el.state = true end)
assert(channels.state(1).off == true and changes == 1 and find(win(), off).state == true and status() == nil)
fire("checked", off, function(el) el.state = false end)
assert(channels.state(1).off == false and changes == 2)
audit_all()

-- the known list: sprite, count, click to tune, disabled by policy
fresh()
ok(channels.set_tx(2, COPPER))
ok(channels.set_tx(3, COPPER))
ok(channels.set_tx(3, IRON))
gui.open(players[1])
local known_copper = find(win(), PREFIX .. "known-" .. COPPER)
assert(known_copper.sprite == "item/copper-plate" and known_copper.number == 1 and known_copper.toggled == false)
assert(find(win(), PREFIX .. "known-" .. ANY).sprite == "virtual-signal/signal-each")
assert(find_caption(win(), "ptt-radio-settings.known-hint"), "the hint says how to create a channel")
fire("click", PREFIX .. "known-" .. COPPER)
assert(channels.tx(1) == COPPER and changes == 1 and find(win(), PREFIX .. "known-" .. COPPER).toggled == true)
assert(find(win(), PREFIX .. "known-" .. COPPER).number == 2)
ok(channels.policy_confine(1, 1, { COPPER }))
gui.refresh(players[1])
assert(not enabled(find(win(), PREFIX .. "known-" .. IRON)) and key_of(find(win(), PREFIX .. "known-" .. IRON).tooltip) == "refused-confined")
fire("click", PREFIX .. "known-" .. IRON)
assert(channels.tx(1) == COPPER and status() == "refused-confined")
-- the honest-clients note is on the window
assert(find_caption(win(), "ptt-radio-settings.honest-short"))
assert(key_of(find_caption(win(), "ptt-radio-settings.honest-short").tooltip) == "honest-note")
audit_all()

-- the admin section callback: called with (player, parent) when provided, absent otherwise
fresh()
local calls = {}
gui.configure(function() changes = changes + 1 end, function(player, parent)
  calls[#calls + 1] = { player = player, parent = parent }
  parent.add({ type = "label", name = "ptt-radio-admin-probe", caption = "admin section" })
end)
gui.open(players[2])
assert(#calls == 1 and calls[1].player == players[2] and calls[1].parent.name == ADMIN)
assert(find(win(2), ADMIN) == calls[1].parent and find(win(2), "ptt-radio-admin-probe"))
gui.refresh(players[2])
assert(#calls == 2 and calls[2].parent ~= calls[1].parent and find(win(2), ADMIN) == calls[2].parent)
assert(gui.on_gui_click({ element = find(win(2), "ptt-radio-admin-probe"), player_index = 2 }) == false)
gui.configure(function() changes = changes + 1 end)
gui.refresh(players[2])
assert(#calls == 2 and find(win(2), ADMIN) == nil)
gui.configure(nil)
fire("click", PREFIX .. "tune-" .. HERE, nil, 2)
assert(channels.tx(2) == HERE and changes == 0, "no callback is fine")

-- a changed audience refreshes every open window, a refusal only the actor's
fresh()
gui.open(players[1])
gui.open(players[2])
local other = find(win(2), PREFIX .. "off")
fire("checked", PREFIX .. "off", function(el) el.state = true end, 1)
assert(find(win(2), PREFIX .. "off") ~= other and find(win(2), PREFIX .. "off").state == false)
assert(find(win(1), PREFIX .. "off").state == true)

-- location survives a rebuild, and the focus follows the window
fresh()
gui.open(players[1])
win().location = { x = 10, y = 20 }
local old = win()
fire("click", PREFIX .. "tune-" .. HERE)
assert(win() ~= old and win().location.x == 10 and players[1].opened == win())
players[1].opened = nil
fire("click", PREFIX .. "tune-" .. ANY)
assert(players[1].opened == nil, "a rebuild does not steal the focus")

-- locale: every key used exists in both languages, the reasons and the controls too
audit_all()
local function locale(lang)
  local keys, section = {}, nil
  for line in io.lines(dir .. "locale/" .. lang .. "/ptt-radio.cfg") do
    local name = line:match("^%[(.+)%]$")
    if name then
      section = name
    elseif section and line:match("^[^=#]+=") then
      keys[section .. "." .. line:match("^([^=]+)=")] = true
    end
  end
  return keys
end
for _, lang in ipairs({ "en", "ru" }) do
  local keys = locale(lang)
  for key in pairs(used) do assert(keys["ptt-radio-settings." .. key], lang .. " lacks " .. key) end
  for _, reason in ipairs({
    "invalid-player", "invalid-channel", "unusable-signal", "locked", "banned", "denied", "confined", "mandatory",
    "too-many", "duplicate", "no-presets", "no-allowed-preset",
  }) do
    assert(keys["ptt-radio-settings.refused-" .. reason], lang .. " lacks refused-" .. reason)
  end
  for _, lock in ipairs({ "tx", "rx", "all" }) do assert(keys["ptt-radio-settings.lock-" .. lock]) end
  assert(keys["shortcut-name.ptt-radio-settings"] and keys["controls.ptt-radio-settings-open"] and keys["controls.ptt-radio-settings-cycle"], lang)
end
local function slurp(path)
  local file = assert(io.open(path))
  local body = file:read("a")
  file:close()
  return body
end
assert(slurp(dir .. "locale/ru/ptt-radio.cfg"):find("ередача запрещена администратором", 1, true))
assert(slurp(dir .. "locale/ru/ptt-radio.cfg"):find("бязательный канал администратора", 1, true))
assert(slurp(dir .. "locale/en/ptt-radio.cfg"):find("honest clients", 1, true))

-- control.lua wiring
fresh()
package.loaded.hud, package.loaded.channels, package.loaded.settings_gui = nil, nil, nil
defines = { events = setmetatable({}, { __index = function(_, key) return key end }) }
script = {
  on_event = function(event, handler)
    for _, name in ipairs(type(event) == "table" and event or { event }) do handlers[name] = handler end
  end,
  on_nth_tick = function() end,
  on_init = function(handler) script.init = handler end,
  on_configuration_changed = function(handler) script.configure = handler end,
}
helpers = {
  send_udp = function(port, data, target) sent[#sent + 1] = { port = port, data = data, target = target } end,
  recv_udp = function() end,
}
commands = { add_command = function() end }
storage = {}
dofile(control_path)
script.init()
gui = package.loaded.settings_gui
channels = package.loaded.channels
local revision = storage.audience_revision
handlers["ptt-radio-settings-open"]({ player_index = 1 })
assert(win() and players[1].opened == win())
handlers["ptt-radio-settings-open"]({ player_index = 1 })
assert(win() == nil)
handlers.on_lua_shortcut({ prototype_name = "ptt-radio-settings", player_index = 1 })
assert(win())
handlers.on_lua_shortcut({ prototype_name = "ptt-radio-settings", player_index = 1 })
assert(win() == nil)
-- the mic shortcut keeps working and opens nothing
handlers.on_lua_shortcut({ prototype_name = "ptt-radio-open-mic", player_index = 1 })
assert(win() == nil and players[1].printed[#players[1].printed][1] == "ptt-radio.no-companion")
handlers.on_lua_shortcut({ prototype_name = "something-else", player_index = 1 })
assert(win() == nil)
-- a gui click is consumed by the settings window, and an audience change follows a mutation
handlers["ptt-radio-settings-open"]({ player_index = 1 })
handlers.on_gui_click({ element = find(win(), PREFIX .. "tune-" .. HERE), player_index = 1 })
assert(channels.tx(1) == HERE and storage.audience_revision ~= revision and #sent > 0)
handlers.on_gui_elem_changed({ element = (function()
  local el = find(win(), PREFIX .. "tx-pick")
  el.elem_value = { type = "fluid", name = "water" }
  return el
end)(), player_index = 1 })
assert(channels.tx(1) == WATER)
handlers.on_gui_checked_state_changed({ element = (function()
  local el = find(win(), PREFIX .. "off")
  el.state = true
  return el
end)(), player_index = 1 })
assert(channels.state(1).off == true)
-- control.lua injects admin.render: an admin's window carries the admin section, and an
-- admin click goes through the shared dispatch and refreshes the open window
assert(find(win(), "ptt-radio-admin-section"), "admin.render is injected into the window")
find(win(), "ptt-radio-admin-target").text = "2"
local stale = win()
handlers.on_gui_click({ element = find(win(), "ptt-radio-admin-mute"), player_index = 1 })
assert(storage.radio_policy.muted[2] and win() and win() ~= stale and find(win(), "ptt-radio-admin-section"))
handlers.on_gui_closed({ element = win(), player_index = 1 })
assert(win() == nil)
handlers["ptt-radio-settings-cycle"]({ player_index = 1 })
assert(key_of(players[1].printed[#players[1].printed][4]) == "refused-no-presets")
-- the echo panel button is untouched
local echo = element(nil, { name = "ptt_radio_echo_open_mic_off" })
storage.open_mic[1] = true
handlers.on_gui_click({ element = echo, player_index = 1 })
assert(storage.open_mic[1] == nil)

print("settings_gui_test ok")

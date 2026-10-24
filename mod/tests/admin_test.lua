-- run from ptt-radio/: lua mod/tests/admin_test.lua
package.path = "mod/ptt-radio/?.lua;" .. package.path
local channels, admin = require("channels"), require("admin")
local PREFIX, LOCALE = "ptt-radio-admin-", "ptt-radio.ptt-radio-admin-"
local IRON, COPPER, WATER = "item/iron-plate", "item/copper-plate", "fluid/water"
local players, command, messages, notifications

local function element(parent, spec)
  local item = { parent = parent, children = {}, style = {}, valid = true, name = spec.name or "" }
  for key, value in pairs(spec) do if key ~= "style" then item[key] = value end end
  item.add = function(child_spec)
    if child_spec.name then assert(child_spec.name:sub(1, #PREFIX) == PREFIX) end
    local child = element(item, child_spec)
    item.children[#item.children + 1] = child
    if child_spec.name then assert(not item[child_spec.name]); item[child_spec.name] = child end
    return child
  end
  item.clear = function()
    while #item.children > 0 do item.children[1].destroy() end
  end
  item.destroy = function()
    item.clear()
    item.valid = false
    if parent then
      if item.name ~= "" then parent[item.name] = nil end
      for i, child in ipairs(parent.children) do
        if child == item then table.remove(parent.children, i); break end
      end
    end
  end
  return item
end

local function setup()
  storage = {}
  settings = { global = { ["ptt-radio-scope"] = { value = "all" } } }
  prototypes = {
    item = { ["iron-plate"] = {}, ["copper-plate"] = {}, parameter = { parameter = true } },
    fluid = { water = {} }, virtual_signal = {}, entity = {}, recipe = {},
    space_location = {}, asteroid_chunk = {}, quality = {},
  }
  messages, notifications, players = {}, 0, {}
  local function receive(actor, text) messages[#messages + 1] = { actor = actor, text = text } end
  for index, name in ipairs({ "Admin", "Player Two", "Third" }) do
    players[index] = { index = index, name = name, admin = index == 1, connected = true,
      force = { index = 1 }, physical_surface = { index = 1 },
      gui = { screen = element(nil, {}) }, print = function(text) receive(index, text) end }
  end
  game = {
    players = players, connected_players = players,
    get_player = function(id)
      assert(id ~= 0, "console is not a LuaPlayer")
      if type(id) == "number" then return players[id] end
      for _, player in pairs(players) do if player.name == id then return player end end
    end,
    print = function(text) receive(0, text) end,
  }
  channels.init()
  commands = { add_command = function(name, help, handler)
    assert(name == "radio-admin" and help[1] == LOCALE .. "command-help")
    command = handler
  end }
  admin.configure(function() notifications = notifications + 1 end)
  admin.register()
end

local function run(text, actor, result)
  local before = notifications
  command({ parameter = text, player_index = actor })
  local message = messages[#messages]
  assert(message.text[1] == LOCALE .. result, message.text[1] .. " != " .. result)
  assert(notifications == before + ((result == "applied" or result == "any-warning") and 1 or 0), "callback count")
end

-- commands share real policy mutations; no callback on refusal or no-op.
setup()
run("mute 2", 2, "not-admin")
assert(not storage.radio_policy.muted[2])
run("mute 2", nil, "applied")
assert(storage.radio_policy.muted[2] and not channels.eligible(2, 3))
run("mute 2", nil, "unchanged")
run('unmute "Player Two"', 1, "applied")
assert(channels.eligible(2, 3))
run("channel fluid/water", 1, "applied")
assert(channels.set_tx(2, WATER))
assert(not channels.eligible(2, 3))
run('publisher add "Player Two"', 1, "applied")
assert(channels.eligible(2, 3))
run("publisher remove 2", nil, "applied")
assert(not channels.eligible(2, 3))
run("channel none", 1, "applied")
assert(not channels.eligible(2, 3))
assert(channels.set_tx(2, IRON)); assert(channels.set_tx(3, IRON))
run("ban iron-plate", 1, "applied")
assert(not channels.eligible(2, 3))
run("unban item/iron-plate", 1, "applied")
assert(channels.eligible(2, 3))
run("pin item/iron-plate", 1, "applied")
assert(storage.radio_policy.pinned[IRON])
run("ban item/iron-plate", 1, "ban-pin")
assert(not storage.radio_policy.banned[IRON] and storage.radio_policy.pinned[IRON])
run("unpin item/iron-plate", 1, "applied")
assert(not storage.radio_policy.pinned[IRON])
run('deny "Player Two" item/iron-plate tx', 1, "applied")
assert(not channels.eligible(2, 3) and channels.eligible(3, 2))
run("deny 2 item/iron-plate rx", 1, "applied")
assert(channels.eligible(2, 3) and not channels.eligible(3, 2))
run("deny 2 item/iron-plate both", 1, "applied")
assert(not channels.eligible(2, 3) and not channels.eligible(3, 2))
run("allow 2 item/iron-plate", 1, "applied")
assert(channels.eligible(2, 3) and channels.eligible(3, 2))
run("lock 2 tx", 1, "applied")
assert(select(2, channels.set_tx(2, COPPER)) == "locked")
run("lock 2 rx", 1, "applied")
assert(select(2, channels.set_rx(2, COPPER, true)) == "locked")
run("lock 2 all", 1, "applied")
run("channel fluid/water", 1, "applied")
assert(channels.set_tx(1, WATER))
assert(channels.eligible(1, 2) and channels.eligible(0, 2))
assert(channels.set_off(2, true))
assert(not channels.eligible(1, 2) and not channels.eligible(0, 2))
assert(channels.set_off(2, false))
run("unlock 2", 1, "applied")
run("confine 2 item/iron-plate HERE", 1, "applied")
assert(select(2, channels.set_tx(2, COPPER)) == "confined")
assert(channels.eligible(1, 2), "mandatory channel survives confinement")
run("release 2", 1, "applied")
assert(channels.set_tx(2, COPPER))
run("channel ANY", 1, "any-warning")
assert(storage.radio_policy.admin_channel == channels.ANY)
assert(channels.set_tx(2, channels.ANY))
assert(not channels.eligible(2, 3), "ordinary ANY publishers are silenced")
run("publisher add 2", 1, "applied")
assert(channels.eligible(2, 3))
run("ban ANY", 1, "mandatory")
run("deny 3 ANY rx", 1, "mandatory")
assert(not storage.radio_policy.banned[channels.ANY] and not storage.radio_policy.deny[3])
run("channel HERE", 1, "here-mandatory")
run("channel none", 1, "applied")
run("ban iron-plate", 1, "applied")
run("confine 2 iron-plate", 1, "confine-banned")
assert(not storage.radio_policy.confine[2])
run("unban iron-plate", 1, "applied")
run("confine 2 iron-plate", 1, "applied")
run("deny 2 iron-plate both", 1, "confine-denied")
assert(not storage.radio_policy.deny[2])
run("release 2", 1, "applied")
run("ban ANY", 1, "applied")
run("lock 2 all", 1, "lock-forbidden")
assert(not storage.radio_policy.lock[2])

-- bad grammar and unresolvable inputs refuse atomically.
setup()
for _, text in ipairs({ "", "unknown", "mute", "mute 2 extra", 'mute "Player Two', 'mute "Player Two"suffix', "publisher nope 2", "confine 2" }) do
  run(text, 1, "usage")
end
for _, target in ipairs({ "0", "-1", "2.5", "99999999999999999", "missing" }) do run("mute " .. target, 1, "invalid-player") end
run("ban item/missing", 1, "invalid-channel")
run("ban item/parameter", 1, "invalid-channel")
run("lock 2 invalid", 1, "invalid-mode")
run("deny 2 iron-plate invalid", 1, "invalid-mode")
assert(notifications == 0 and next(storage.radio) == nil)

local function find(root, suffix)
  if root.name == PREFIX .. suffix then return root end
  for _, child in ipairs(root.children) do
    local found = find(child, suffix)
    if found then return found end
  end
end
local function click(root, op, actor, expected)
  local before = notifications
  assert(admin.on_gui_click({ element = find(root, op), player_index = actor or 1 }))
  assert(messages[#messages].text[1] == LOCALE .. expected, messages[#messages].text[1])
  assert(notifications == before + ((expected == "applied" or expected == "any-warning") and 1 or 0))
end

-- editor uses the same mutations, target grammar, and actual-mutation callback.
setup()
admin.render(players[2], players[2].gui.screen)
assert(#players[2].gui.screen.children == 0)
admin.render(players[1], players[1].gui.screen)
local root = players[1].gui.screen[PREFIX .. "section"]
assert(root and find(root, "current"))
find(root, "target").text = "Player Two"
find(root, "signal").elem_value = { type = "fluid", name = "water" }
assert(admin.on_gui_elem_changed({ element = find(root, "signal"), player_index = 1 }))
assert(find(root, "kind").selected_index == 1)
click(root, "channel", 1, "applied")
assert(storage.radio_policy.admin_channel == WATER)
click(root, "channel", 1, "unchanged")
click(root, "ban", 1, "mandatory")
click(root, "clear-channel", 1, "applied")
click(root, "pin", 1, "applied"); click(root, "unpin", 1, "applied")
click(root, "ban", 1, "applied"); click(root, "unban", 1, "applied")
click(root, "mute", 1, "applied"); click(root, "unmute", 1, "applied")
click(root, "publisher-add", 1, "applied"); click(root, "publisher-remove", 1, "applied")
click(root, "deny", 1, "applied")
assert(storage.radio_policy.deny[2][WATER] == "both")
click(root, "allow", 1, "applied")
click(root, "lock", 1, "applied")
assert(storage.radio_policy.lock[2] == "all")
click(root, "unlock", 1, "applied")
find(root, "confine-keys").text = "ANY item/iron-plate HERE"
click(root, "confine", 1, "applied")
assert(storage.radio_policy.confine[2][IRON] and storage.radio_policy.confine[2][channels.HERE])
click(root, "release", 1, "applied")
find(root, "kind").selected_index = 2
click(root, "channel", 1, "any-warning")
find(root, "kind").selected_index = 3
click(root, "channel", 1, "here-mandatory")
find(root, "signal").elem_value = nil
assert(admin.on_gui_elem_changed({ element = find(root, "signal"), player_index = 1 }))
click(root, "ban", 1, "invalid-channel")

-- authority is live: stale admin widgets and console-shaped gui events cannot mutate.
local mute = find(root, "mute")
players[1].admin = false
local before = notifications
assert(admin.on_gui_click({ element = mute, player_index = 1 }))
assert(not root.valid and notifications == before and not storage.radio_policy.muted[2])
players[1].admin = true
admin.render(players[1], players[1].gui.screen)
root = players[1].gui.screen[PREFIX .. "section"]
find(root, "target").text = "2"
assert(admin.on_gui_click({ element = find(root, "mute"), player_index = 0 }))
assert(not root.valid and notifications == before and not storage.radio_policy.muted[2])
admin.render(players[1], players[1].gui.screen)
root = players[1].gui.screen[PREFIX .. "section"]
players[1].admin = false
assert(admin.on_gui_elem_changed({ element = find(root, "signal"), player_index = 1 }))
assert(not root.valid and notifications == before)
assert(not admin.on_gui_click({ element = element(nil, { name = "other-mod" }), player_index = 1 }))
assert(not admin.on_gui_elem_changed({ element = { valid = false }, player_index = 1 }))

-- an integration callback may rebuild the settings window during a policy change.
players[1].admin = true
admin.render(players[1], players[1].gui.screen)
root = players[1].gui.screen[PREFIX .. "section"]
find(root, "target").text = "2"
admin.configure(function() notifications = notifications + 1; root.destroy() end)
click(root, "mute", 1, "applied")
assert(not root.valid and storage.radio_policy.muted[2])

print("admin.lua: command and gui operations, policy refusals, authority, rollback and callbacks passed")

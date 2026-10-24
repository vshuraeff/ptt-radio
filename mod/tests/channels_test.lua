-- run from ptt-radio/: lua mod/tests/channels_test.lua
package.path = "mod/ptt-radio/?.lua;" .. package.path
local channels = require("channels")
local ANY, HERE = channels.ANY, channels.HERE
local IRON, COPPER, VULCANUS = "item/iron-plate", "item/copper-plate", "space-location/vulcanus"
local WATER, SIG = "fluid/water", "virtual/signal-A"
assert(channels.ADMIN == nil, "the admin channel is a policy, not a fixed key")

local players
local function setup(count)
  players = {}
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
    connected_players = {},
    get_player = function(index) assert(index ~= 0, "player zero is not a LuaPlayer"); return players[index] end,
  }
  for index = 1, count or 4 do
    players[index] = {
      index = index, connected = true, admin = false,
      force = { index = 1 }, physical_surface = { index = 1 },
    }
    game.connected_players[#game.connected_players + 1] = players[index]
  end
  players[1].admin = true
  channels.init()
end

-- whom listener hears, as the sorted receive_from u4 builds from eligible
local function heard_by(listener)
  local out = {}
  if channels.eligible(0, listener) then out[#out + 1] = 0 end
  for index = 1, #players do
    if channels.eligible(index, listener) then out[#out + 1] = index end
  end
  return table.concat(out, ",")
end
local function expect_heard(listener, expected)
  local actual = heard_by(listener)
  assert(actual == expected, "listener " .. listener .. " hears " .. actual .. ", expected " .. expected)
end
local function ok(changed, reason) assert(changed == true, tostring(reason)) end
local function refused(expected, changed, reason)
  assert(changed == false and reason == expected, tostring(reason) .. " != " .. expected)
end
local function same_policy(before)
  local function dump(t)
    if type(t) ~= "table" then return tostring(t) end
    local keys = {}
    for k in pairs(t) do keys[#keys + 1] = k end
    table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
    local parts = {}
    for _, k in ipairs(keys) do parts[#parts + 1] = tostring(k) .. "=" .. dump(t[k]) end
    return "{" .. table.concat(parts, ",") .. "}"
  end
  local now = dump(storage.radio_policy)
  if before then assert(now == before, now .. " != " .. before) end
  return now
end

-- keys and sprites
setup()
assert(channels.key({ name = "iron-plate" }) == IRON)
assert(channels.key({ type = "item", name = "iron-plate", quality = "rare" }) == IRON)
assert(channels.key({ type = "fluid", name = "water" }) == "fluid/water")
assert(channels.key({ type = "virtual", name = "signal-A" }) == "virtual/signal-A")
assert(channels.key({ type = "space-location", name = "vulcanus" }) == VULCANUS)
assert(channels.key({ type = "entity", name = "oil-refinery" }) == "entity/oil-refinery")
assert(channels.key({ type = "quality", name = "rare" }) == "quality/rare")
for _, bad in ipairs({
  {}, { type = "item" }, { name = "" }, { type = "item", name = "item-parameter-0" },
  { type = "virtual", name = "signal-each" }, { type = "virtual", name = "signal-anything" },
  { type = "virtual", name = "signal-everything" },
  { name = "missing" }, { type = "tile", name = "iron-plate" }, { type = "recipe", name = "iron-plate" },
}) do
  assert(channels.key(bad) == nil)
end
assert(channels.key(nil) == nil)
assert(channels.sprite(ANY) == "virtual-signal/signal-each")
assert(channels.sprite(HERE) == "virtual-signal/signal-everything")
assert(channels.sprite("ptt/admin") == nil)
assert(channels.sprite(IRON) == "item/iron-plate")
assert(channels.sprite("virtual/signal-A") == "virtual-signal/signal-A")
assert(channels.sprite(VULCANUS) == "space-location/vulcanus")

-- init is idempotent and keeps what is there
storage.radio[2] = { tx = IRON, rx = {}, presets = {} }
storage.radio_policy.muted[3] = true
storage.radio_policy.deny = nil
channels.init()
assert(storage.radio[2].tx == IRON and storage.radio_policy.muted[3] and storage.radio_policy.deny)

-- defaults: everybody on ANY hears everybody, self included, and the server
setup()
assert(channels.tx(2) == ANY)
local s = channels.state(2)
assert(s.tx == ANY and #s.rx == 0 and s.off == false and #s.presets == 0)
expect_heard(1, "0,1,2,3,4")
assert(not channels.eligible(1, 0) and not channels.eligible(0, 0))
assert(next(storage.radio) == nil, "reads never create records")

-- two quiet groups: 1,2 on iron and 3,4 on copper hear only their own group
setup()
ok(channels.set_tx(1, IRON)); ok(channels.set_tx(2, IRON))
ok(channels.set_tx(3, COPPER)); ok(channels.set_tx(4, COPPER))
expect_heard(1, "0,1,2"); expect_heard(2, "0,1,2")
expect_heard(3, "0,3,4"); expect_heard(4, "0,3,4")
-- a group that keeps ANY as an extra receive channel still hears ANY talkers
ok(channels.set_tx(4, ANY))
expect_heard(3, "0,3"); expect_heard(1, "0,1,2")
ok(channels.set_rx(1, ANY, true))
expect_heard(1, "0,1,2,4")
assert(not channels.eligible(1, 4), "rx is additive for the listener only")
assert(channels.set_rx(1, ANY, true) == false)
ok(channels.set_rx(1, ANY, false))
assert(#channels.state(1).rx == 0)
-- rx of one's own tx is stored as an extra, so it survives the next tx change
ok(channels.set_rx(2, IRON, true)); ok(channels.set_tx(2, COPPER))
expect_heard(2, "0,1,2,3")

-- a cross-planet team on a planet channel, and «Здесь» on physical surfaces
setup()
players[2].physical_surface = { index = 2 }
ok(channels.set_tx(1, VULCANUS)); ok(channels.set_tx(2, VULCANUS))
expect_heard(1, "0,1,2"); expect_heard(2, "0,1,2")
ok(channels.set_tx(1, HERE)); ok(channels.set_tx(2, HERE)); ok(channels.set_tx(3, HERE))
expect_heard(1, "0,1,3"); expect_heard(2, "0,2"); expect_heard(3, "0,1,3")
-- «Здесь» as an extra rx reaches only the same surface, and follows travel
ok(channels.set_rx(4, HERE, true))
expect_heard(4, "0,1,3,4")
players[2].physical_surface = { index = 1 }
expect_heard(4, "0,1,2,3,4")

-- overlap gives one stream: the sender is heard once whatever the route
setup()
ok(channels.set_tx(1, IRON)); ok(channels.set_tx(2, IRON))
ok(channels.set_rx(2, IRON, true)); ok(channels.set_rx(2, ANY, true))
expect_heard(2, "0,1,2,3,4")
-- still one entry once the sender moves to the mandatory admin channel
ok(channels.policy_admin_channel(0, WATER)); ok(channels.set_tx(1, WATER))
expect_heard(2, "0,1,2,3,4")

-- radio off is fully off: no server, no admin channel, no self; an off sender is silent
setup()
ok(channels.policy_admin_channel(1, WATER)); ok(channels.set_tx(2, WATER)); ok(channels.policy_publisher(1, 2, true))
expect_heard(3, "0,1,2,3,4")
ok(channels.set_off(3, true))
assert(channels.state(3).off)
expect_heard(3, "")
assert(not channels.eligible(0, 3) and not channels.eligible(2, 3) and not channels.eligible(3, 3))
assert(channels.set_off(3, true) == false)
ok(channels.set_off(2, true))
expect_heard(1, "0,1,4")
ok(channels.set_off(3, false)); ok(channels.set_off(2, false))
expect_heard(3, "0,1,2,3,4")

-- no admin channel by default; unset means the ordinary rules, publishers or not
setup()
assert(storage.radio_policy.admin_channel == nil)
ok(channels.policy_publisher(1, 2, true))
ok(channels.set_tx(2, WATER)); ok(channels.set_tx(3, IRON))
expect_heard(3, "0,3"); expect_heard(1, "0,1,4")
ok(channels.policy_admin_channel(1, WATER))
assert(storage.radio_policy.admin_channel == WATER)
expect_heard(3, "0,2,3")
assert(channels.policy_admin_channel(1, WATER) == false, "same channel is unchanged")
ok(channels.policy_admin_channel(1, nil))
assert(storage.radio_policy.admin_channel == nil)
expect_heard(3, "0,3")
-- an unset channel does not retain the publisher special case
ok(channels.policy_publisher(1, 2, false))
ok(channels.set_tx(1, WATER))
expect_heard(1, "0,1,2"); expect_heard(3, "0,3")
refused("not-admin", channels.policy_admin_channel(2, WATER))
refused("invalid-channel", channels.policy_admin_channel(1, "item/missing"))
refused("invalid-channel", channels.policy_admin_channel(1, "ptt/nope"))
refused("invalid-channel", channels.policy_admin_channel(1, "virtual/signal-each"))

-- a signal key as the admin channel: heard by everyone with a radio on, from any channel
setup()
ok(channels.set_tx(3, IRON)); ok(channels.set_tx(4, COPPER)); ok(channels.set_rx(4, VULCANUS, true))
ok(channels.policy_admin_channel(0, SIG))
ok(channels.policy_publisher(1, 2, true)); ok(channels.set_tx(2, SIG))
expect_heard(3, "0,2,3"); expect_heard(4, "0,2,4"); expect_heard(1, "0,1,2")
-- a non-publisher non-admin on that tx is silent, without losing the selection
ok(channels.set_tx(3, SIG))
assert(channels.tx(3) == SIG)
expect_heard(4, "0,2,4")
-- a player.admin sender is heard without being a publisher
players[3].admin = true
expect_heard(4, "0,2,3,4")
players[3].admin = false
-- dropping the publisher silences it at once
ok(channels.policy_publisher(1, 2, false))
expect_heard(4, "0,4")
-- radio off silences the admin channel for that listener only
ok(channels.policy_publisher(1, 2, true))
ok(channels.set_off(4, true))
expect_heard(4, ""); expect_heard(3, "0,2")
ok(channels.set_off(4, false))
-- mute, rx unsubscribe, deny and ban cannot defeat it on the listener
ok(channels.policy_mute(0, 2, true))
expect_heard(3, "0"); ok(channels.policy_mute(0, 2, false))
refused("mandatory", channels.set_rx(3, SIG, false))
ok(channels.set_rx(3, SIG, true)); refused("mandatory", channels.set_rx(3, SIG, false))
ok(channels.policy_pin(1, SIG, true))
expect_heard(4, "0,2,4")
-- ban and deny of the designated channel are refused and leave the policy untouched
local before = same_policy()
refused("mandatory", channels.policy_ban(1, SIG, true)); same_policy(before)
refused("mandatory", channels.policy_deny(1, 3, SIG, "rx")); same_policy(before)
refused("mandatory", channels.policy_deny(1, 3, SIG, "both")); same_policy(before)
-- and the other way round: a banned or denied channel cannot be designated
ok(channels.policy_admin_channel(1, nil))
ok(channels.policy_ban(1, IRON, true)); ok(channels.policy_deny(1, 3, COPPER, "tx"))
before = same_policy()
refused("mandatory", channels.policy_admin_channel(1, IRON)); same_policy(before)
refused("mandatory", channels.policy_admin_channel(1, COPPER)); same_policy(before)
refused("here-mandatory", channels.policy_admin_channel(1, HERE)); same_policy(before)
assert(storage.radio_policy.admin_channel == nil)
-- confinement does not hide it from a confined listener
ok(channels.policy_ban(1, IRON, false)); ok(channels.policy_deny(1, 3, COPPER, nil))
ok(channels.policy_admin_channel(1, SIG)); ok(channels.policy_confine(1, 4, { COPPER }))
expect_heard(4, "0,2,4")

-- ptt/any as the admin channel: everyone hears it, only publishers and admins speak on it
setup()
players[1].admin = false
ok(channels.set_tx(2, IRON)); ok(channels.set_tx(3, IRON))
ok(channels.policy_admin_channel(0, ANY))
ok(channels.policy_publisher(0, 4, true))
-- players 1 and 4 keep the default ANY tx; only the publisher 4 is heard
expect_heard(2, "0,2,3,4"); expect_heard(3, "0,2,3,4")
expect_heard(4, "0,4"); expect_heard(1, "0,4")
ok(channels.policy_admin_channel(0, nil))
expect_heard(4, "0,1,4"); expect_heard(2, "0,2,3")

-- force scope is the outer boundary, also for the admin channel; player 0 crosses it
setup()
players[3].force = { index = 2 }; players[4].force = { index = 2 }
expect_heard(1, "0,1,2,3,4")
settings.global["ptt-radio-scope"].value = "force"
expect_heard(1, "0,1,2"); expect_heard(3, "0,3,4")
ok(channels.policy_admin_channel(0, WATER)); ok(channels.set_tx(1, WATER))
expect_heard(3, "0,3,4"); expect_heard(2, "0,1,2")

-- player mutations: changed, unchanged, refusals
setup()
assert(channels.set_tx(2, ANY) == false)
refused("invalid-channel", channels.set_tx(2, "item/missing"))
refused("invalid-channel", channels.set_tx(2, "item/item-parameter-0"))
refused("invalid-channel", channels.set_tx(2, "ptt/nope"))
refused("invalid-channel", channels.set_rx(2, 42, true))
refused("invalid-player", channels.set_tx(0, IRON))
refused("invalid-player", channels.set_tx(9, IRON))
refused("invalid-player", channels.set_off(0, true))
refused("too-many", channels.set_presets(2, { ANY, HERE, IRON, COPPER, VULCANUS, "fluid/water", "virtual/signal-A", "quality/rare", "entity/oil-refinery" }))
refused("duplicate", channels.set_presets(2, { IRON, IRON }))
refused("invalid-channel", channels.set_presets(2, { IRON, "item/missing" }))
assert(#channels.state(2).presets == 0)
refused("no-presets", channels.cycle(2))
ok(channels.set_presets(2, { ANY, HERE, IRON, COPPER, VULCANUS, "fluid/water", "virtual/signal-A", "quality/rare" }))
assert(channels.set_presets(2, { ANY, HERE, IRON, COPPER, VULCANUS, "fluid/water", "virtual/signal-A", "quality/rare" }) == false)
ok(channels.set_presets(2, { IRON, COPPER, VULCANUS }))
-- cycle starts at the first preset when tx is not one of them, then wraps, skipping refused ones
ok(channels.cycle(2)); assert(channels.tx(2) == IRON)
ok(channels.cycle(2)); assert(channels.tx(2) == COPPER)
ok(channels.policy_ban(1, VULCANUS, true))
ok(channels.cycle(2)); assert(channels.tx(2) == IRON)
ok(channels.set_presets(2, { IRON, VULCANUS }))
refused("no-allowed-preset", channels.cycle(2))
local presets = channels.state(2).presets
presets[1] = COPPER
assert(channels.state(2).presets[1] == IRON, "state is a copy")

-- admin authority: index 0 is the console, everyone else needs player.admin
setup()
refused("not-admin", channels.policy_mute(2, 3, true))
refused("not-admin", channels.policy_ban(2, IRON, true))
refused("not-admin", channels.policy_ban(9, IRON, true))
ok(channels.policy_mute(0, 3, true))
ok(channels.policy_mute(1, 4, true))
refused("server", channels.policy_mute(1, 0, true))
refused("not-admin", channels.policy_mute(2, 0, true))
refused("invalid-player", channels.policy_mute(1, 9, true))
refused("invalid-mode", channels.policy_lock(1, 2, "everything"))
refused("invalid-mode", channels.policy_deny(1, 2, IRON, "speak"))
refused("invalid-channel", channels.policy_ban(1, "item/missing", true))
assert(channels.policy_mute(1, 4, true) == false)
-- mute silences tx only; the muted player still hears
expect_heard(1, "0,1,2")
expect_heard(3, "0,1,2")
-- even a muted publisher on the admin channel
ok(channels.policy_admin_channel(0, WATER)); ok(channels.policy_publisher(0, 3, true)); ok(channels.set_tx(3, WATER))
expect_heard(2, "0,1,2")
ok(channels.policy_mute(0, 3, false))
expect_heard(2, "0,1,2,3")

-- global ban: refuses selections and silences existing ones at runtime
setup()
ok(channels.set_tx(2, IRON)); ok(channels.set_tx(3, IRON))
expect_heard(3, "0,2,3")
ok(channels.policy_ban(1, IRON, true))
expect_heard(3, "0")
refused("banned", channels.set_tx(4, IRON))
refused("banned", channels.set_rx(4, IRON, true))
ok(channels.set_tx(2, ANY))
ok(channels.policy_ban(1, IRON, false))
expect_heard(3, "0,3")

-- per-player deny: speak, listen or both
setup()
ok(channels.set_tx(1, IRON)); ok(channels.set_tx(2, IRON)); ok(channels.set_tx(3, IRON))
ok(channels.policy_deny(1, 2, IRON, "tx"))
refused("denied", channels.set_tx(2, IRON))
expect_heard(3, "0,1,3"); expect_heard(2, "0,1,3")
ok(channels.policy_deny(1, 2, IRON, "rx"))
expect_heard(2, "0"); expect_heard(3, "0,1,2,3")
refused("denied", channels.set_rx(2, IRON, true))
ok(channels.policy_deny(1, 2, IRON, "both"))
expect_heard(2, "0"); expect_heard(3, "0,1,3")
ok(channels.policy_deny(1, 2, IRON, nil))
assert(storage.radio_policy.deny[2] == nil, "an emptied deny bucket is dropped")
expect_heard(2, "0,1,2,3")

-- locks of tx, rx or all changes
setup()
ok(channels.set_presets(2, { IRON, COPPER }))
ok(channels.policy_lock(1, 2, "tx"))
refused("locked", channels.set_tx(2, IRON))
refused("locked", channels.cycle(2))
ok(channels.set_rx(2, IRON, true))
ok(channels.set_off(2, true)); ok(channels.set_off(2, false))
ok(channels.policy_lock(1, 2, "rx"))
refused("locked", channels.set_rx(2, IRON, false))
ok(channels.set_tx(2, IRON))
ok(channels.policy_lock(1, 2, "all"))
refused("locked", channels.set_tx(2, COPPER))
refused("locked", channels.set_rx(2, COPPER, true))
refused("locked", channels.set_presets(2, {}))
refused("locked", channels.cycle(2))
-- the lock never covers the on/off switch; off silences the admin channel and player 0
ok(channels.policy_admin_channel(1, WATER)); ok(channels.policy_publisher(1, 3, true)); ok(channels.set_tx(3, WATER))
ok(channels.set_off(2, true))
local heard_off = heard_by(2)
assert(not heard_off:find("0", 1, true) and not heard_off:find("3", 1, true), "off player hears " .. heard_off)
refused("locked", channels.set_tx(2, COPPER))
refused("locked", channels.set_rx(2, COPPER, true))
refused("locked", channels.set_presets(2, {}))
ok(channels.set_off(2, false))
ok(channels.policy_lock(1, 2, nil))
ok(channels.set_off(2, true))

-- confinement to a permitted set; the admin channel is still heard inside it
setup()
ok(channels.set_tx(3, COPPER))
ok(channels.policy_confine(1, 2, { IRON, HERE }))
refused("confined", channels.set_tx(2, COPPER))
refused("confined", channels.set_rx(2, COPPER, true))
expect_heard(2, "0")
ok(channels.set_tx(2, IRON)); ok(channels.set_tx(1, IRON))
expect_heard(2, "0,1,2")
ok(channels.policy_admin_channel(1, WATER)); ok(channels.policy_publisher(1, 4, true)); ok(channels.set_tx(4, WATER))
expect_heard(2, "0,1,2,4")
assert(channels.policy_confine(1, 2, { HERE, IRON }) == false, "same set is unchanged")
ok(channels.policy_confine(1, 2, nil))
ok(channels.set_tx(2, COPPER))

-- contradictory policy is rejected and leaves the policy untouched
setup()
ok(channels.policy_ban(1, IRON, true))
local before = same_policy()
refused("ban-pin", channels.policy_pin(1, IRON, true)); same_policy(before)
ok(channels.policy_pin(1, COPPER, true))
before = same_policy()
refused("ban-pin", channels.policy_ban(1, COPPER, true)); same_policy(before)
refused("confine-empty", channels.policy_confine(1, 2, {})); same_policy(before)
refused("confine-banned", channels.policy_confine(1, 2, { IRON, COPPER })); same_policy(before)
ok(channels.policy_deny(1, 2, COPPER, "both"))
before = same_policy()
refused("confine-denied", channels.policy_confine(1, 2, { COPPER, ANY })); same_policy(before)
ok(channels.policy_confine(1, 3, { COPPER, ANY }))
before = same_policy()
refused("confine-denied", channels.policy_deny(1, 3, COPPER, "both")); same_policy(before)
refused("confine-banned", channels.policy_ban(1, ANY, true)); same_policy(before)
ok(channels.policy_deny(1, 3, COPPER, "rx"))
-- a lock on a tx the policy forbids, in either order
ok(channels.set_tx(4, VULCANUS)); ok(channels.policy_lock(1, 4, "tx"))
refused("lock-forbidden", channels.policy_ban(1, VULCANUS, true))
refused("lock-forbidden", channels.policy_deny(1, 4, VULCANUS, "tx"))
refused("lock-forbidden", channels.policy_confine(1, 4, { ANY }))
ok(channels.policy_lock(1, 4, "rx"))
ok(channels.policy_deny(1, 4, VULCANUS, "tx"))
refused("lock-forbidden", channels.policy_lock(1, 4, "all"))

-- known: channels with connected radio-on members in scope, plus pinned, sorted, with counts
setup(5)
ok(channels.set_tx(1, IRON)); ok(channels.set_tx(2, IRON)); ok(channels.set_rx(3, IRON, true))
ok(channels.set_tx(3, HERE)); ok(channels.set_tx(4, HERE))
players[4].physical_surface = { index = 2 }
ok(channels.set_tx(5, COPPER)); ok(channels.set_off(5, true))
ok(channels.policy_pin(1, VULCANUS, true))
ok(channels.policy_admin_channel(1, WATER)); ok(channels.set_rx(2, ANY, true)); ok(channels.set_tx(2, WATER))
local function listing(viewer)
  local parts = {}
  for _, entry in ipairs(channels.known(viewer)) do
    parts[#parts + 1] = entry.key .. "=" .. entry.count .. (entry.admin and "!" or "")
    assert(entry.admin == (entry.key == WATER or nil), "only the admin channel is flagged")
  end
  return table.concat(parts, " ")
end
assert(listing(1) == "fluid/water=1! item/iron-plate=2 ptt/any=1 ptt/here=1 space-location/vulcanus=0", listing(1))
assert(listing(4) == "fluid/water=1! item/iron-plate=2 ptt/any=1 ptt/here=1 space-location/vulcanus=0", listing(4))
players[2].connected = false
table.remove(game.connected_players, 2)
assert(listing(1) == "fluid/water=0! item/iron-plate=2 ptt/here=1 space-location/vulcanus=0", listing(1))
players[3].force = { index = 2 }
settings.global["ptt-radio-scope"].value = "force"
assert(listing(1) == "fluid/water=0! item/iron-plate=1 space-location/vulcanus=0", listing(1))
assert(listing(3) == "fluid/water=0! item/iron-plate=1 ptt/here=1 space-location/vulcanus=0", listing(3))
ok(channels.policy_ban(1, IRON, true))
assert(listing(3) == "fluid/water=0! ptt/here=1 space-location/vulcanus=0", listing(3))

print("channels.lua: keys, storage, predicate matrix, mutations, policy precedence and known passed")

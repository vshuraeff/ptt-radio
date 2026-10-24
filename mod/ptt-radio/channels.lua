-- ptt-radio channels: the radar-style channel model, with no event wiring.
--
-- a channel is a canonical "type/name" key mirroring a radar universe channel,
-- or one of the reserved "ptt/any" and "ptt/here" keys. each player has one tx
-- channel and hears tx + extra rx subscriptions while the radio is on.
--
-- the admin channel is any existing channel (any key but "ptt/here") that the server
-- admin designates as mandatory: while the radio is on every player hears it and cannot
-- unsubscribe, and only publishers and admins can speak on it. it is not set by default.
--
-- storage.radio[player_index] = { tx = key, rx = { [key] = true }, off = true|nil, presets = { key, ... } }
--   a player without a record has the defaults: tx ANY, no extra rx, radio on, no presets.
-- storage.radio_policy = {
--   muted = { [p] = true }, banned = { [key] = true }, pinned = { [key] = true },
--   deny = { [p] = { [key] = "tx"|"rx"|"both" } }, lock = { [p] = "tx"|"rx"|"all" },
--   confine = { [p] = { [key] = true } }, publishers = { [p] = true },
--   admin_channel = key|nil,
-- }
--
-- locks cover channel and preset selection, never the on/off switch: a player can always
-- switch the whole radio off, the admin channel included; the admin's tool against that is mute.
--
-- refusal reasons: invalid-player, invalid-channel, invalid-mode, not-admin, server,
-- locked, banned, denied, confined, not-publisher, mandatory, here-mandatory, too-many,
-- duplicate, no-presets, no-allowed-preset; contradictions: ban-pin, confine-empty,
-- confine-banned, confine-denied, lock-forbidden, mandatory (the admin channel banned
-- or denied to a player).
--
-- globals (storage, game, settings, prototypes) are read at call time, never captured.

local channels = {}

channels.ANY = "ptt/any"
channels.HERE = "ptt/here"

local MAX_PRESETS = 8
local POLICY_TABLES = { "muted", "banned", "pinned", "deny", "lock", "confine", "publishers" }

-- SignalIDType -> sprite type; the LuaPrototypes table is the sprite type with "_" for "-"
local SPRITE_TYPE = {
  item = "item", fluid = "fluid", virtual = "virtual-signal", entity = "entity", recipe = "recipe",
  ["space-location"] = "space-location", ["asteroid-chunk"] = "asteroid-chunk", quality = "quality",
}
local RESERVED_SPRITE = {
  [channels.ANY] = "virtual-signal/signal-each",
  [channels.HERE] = "virtual-signal/signal-everything",
}
-- virtual signals that stand for a concept of their own; the pseudo keys name those
local SPECIAL_SIGNAL = {
  ["signal-each"] = true, ["signal-anything"] = true, ["signal-everything"] = true,
}
local DENY_MODES = { tx = true, rx = true, both = true }
local LOCK_MODES = { tx = true, rx = true, all = true }
local DEFAULT = { tx = channels.ANY, rx = {}, presets = {} }

local function sorted_keys(set)
  local out = {}
  for k in pairs(set) do out[#out + 1] = k end
  table.sort(out)
  return out
end

function channels.key(signal)
  if type(signal) ~= "table" or type(signal.name) ~= "string" or signal.name == "" then return nil end
  local signal_type = signal.type or "item"
  if signal_type == "virtual" and SPECIAL_SIGNAL[signal.name] then return nil end
  local sprite_type = SPRITE_TYPE[signal_type]
  local proto = sprite_type and prototypes[string.gsub(sprite_type, "-", "_")][signal.name]
  if not proto or proto.parameter then return nil end
  return signal_type .. "/" .. signal.name
end

local function valid_key(key)
  if type(key) ~= "string" then return false end
  if RESERVED_SPRITE[key] then return true end
  local signal_type, name = string.match(key, "^([^/]+)/(.+)$")
  return signal_type ~= nil and channels.key({ type = signal_type, name = name }) == key
end

function channels.sprite(key)
  if RESERVED_SPRITE[key] then return RESERVED_SPRITE[key] end
  local signal_type, name = string.match(key, "^([^/]+)/(.+)$")
  local sprite_type = signal_type and SPRITE_TYPE[signal_type]
  return sprite_type and sprite_type .. "/" .. name or nil
end

function channels.init()
  storage.radio = storage.radio or {}
  storage.radio_policy = storage.radio_policy or {}
  for _, name in ipairs(POLICY_TABLES) do
    storage.radio_policy[name] = storage.radio_policy[name] or {}
  end
end

local function record(p)
  return storage.radio[p] or DEFAULT
end

local function own(p)
  local r = storage.radio[p]
  if not r then
    r = { tx = channels.ANY, rx = {}, presets = {} }
    storage.radio[p] = r
  end
  return r
end

local function is_player(p)
  return p ~= 0 and type(p) == "number" and game.get_player(p) ~= nil
end

-- why player p may not select key as tx, or nil; the admin channel can be selected by
-- anyone, and eligible silences a sender that is neither publisher nor admin
local function tx_refusal(p, key)
  local policy = storage.radio_policy
  if key == policy.admin_channel then return nil end
  if policy.banned[key] then return "banned" end
  local mode = policy.deny[p] and policy.deny[p][key]
  if mode == "tx" or mode == "both" then return "denied" end
  if policy.confine[p] and not policy.confine[p][key] then return "confined" end
  return nil
end

local function rx_refusal(p, key)
  local policy = storage.radio_policy
  if key == policy.admin_channel then return nil end
  if policy.banned[key] then return "banned" end
  local mode = policy.deny[p] and policy.deny[p][key]
  if mode == "rx" or mode == "both" then return "denied" end
  if policy.confine[p] and not policy.confine[p][key] then return "confined" end
  return nil
end

local function receives(p, key)
  if key == storage.radio_policy.admin_channel then return true end
  local r = record(p)
  return (r.tx == key or r.rx[key] == true) and rx_refusal(p, key) == nil
end

-- precedence: listener presence > listener off (silences player 0, the admin channel
-- and self too) > server sender > presence > force scope > sender off > mute > tx
-- permission > listener rx > «Здесь» surface. every rule restricts, so the order only
-- matters for the exemptions: player 0 skips everything after listener off, and the
-- admin channel, when one is set, skips tx, rx, ban, deny and confinement on the
-- listener, while its sender needs the publisher allowlist or player.admin instead of
-- the ordinary tx checks.
function channels.tx_permitted(sender_index)
  if sender_index == 0 then return true end
  local sender = game.get_player(sender_index)
  if not sender or not sender.connected then return false end
  local r = record(sender_index)
  if r.off or storage.radio_policy.muted[sender_index] then return false end
  if r.tx == storage.radio_policy.admin_channel then
    return storage.radio_policy.publishers[sender_index] == true or sender.admin == true
  end
  return tx_refusal(sender_index, r.tx) == nil
end

function channels.eligible(sender_index, listener_index)
  if listener_index == 0 then return false end
  local listener = game.get_player(listener_index)
  if not listener or not listener.connected or record(listener_index).off then return false end
  if sender_index == 0 then return true end
  if not channels.tx_permitted(sender_index) then return false end
  local sender = game.get_player(sender_index)
  if settings.global["ptt-radio-scope"].value == "force" and sender.force.index ~= listener.force.index then
    return false
  end
  local r = record(sender_index)
  if not receives(listener_index, r.tx) then return false end
  if r.tx == channels.HERE then
    return sender.physical_surface.index == listener.physical_surface.index
  end
  return true
end

function channels.tx(p)
  return record(p).tx
end

-- a copy for display: rx extras sorted, presets in order
function channels.state(p)
  local r = record(p)
  local presets = {}
  for i, key in ipairs(r.presets) do presets[i] = key end
  return { tx = r.tx, rx = sorted_keys(r.rx), off = r.off == true, presets = presets }
end

local function lock_refusal(p, scope)
  local mode = storage.radio_policy.lock[p]
  return (mode == "all" or mode == scope) and "locked" or nil
end

function channels.set_tx(p, key)
  if not is_player(p) then return false, "invalid-player" end
  if not valid_key(key) then return false, "invalid-channel" end
  local reason = lock_refusal(p, "tx") or tx_refusal(p, key)
  if reason then return false, reason end
  if record(p).tx == key then return false end
  own(p).tx = key
  return true
end

function channels.set_rx(p, key, on)
  if not is_player(p) then return false, "invalid-player" end
  if not valid_key(key) then return false, "invalid-channel" end
  local reason = lock_refusal(p, "rx") or (not on and key == storage.radio_policy.admin_channel and "mandatory")
    or (on and rx_refusal(p, key))
  if reason then return false, reason end
  on = on and true or nil
  if record(p).rx[key] == on then return false end
  own(p).rx[key] = on
  return true
end

function channels.set_off(p, off)
  if not is_player(p) then return false, "invalid-player" end
  off = off and true or nil
  if record(p).off == off then return false end
  own(p).off = off
  return true
end

function channels.set_presets(p, keys)
  if not is_player(p) then return false, "invalid-player" end
  local reason = lock_refusal(p, "all")
  if reason then return false, reason end
  if #keys > MAX_PRESETS then return false, "too-many" end
  local seen, changed = {}, #keys ~= #record(p).presets
  for i, key in ipairs(keys) do
    if not valid_key(key) then return false, "invalid-channel" end
    if seen[key] then return false, "duplicate" end
    seen[key] = true
    changed = changed or record(p).presets[i] ~= key
  end
  if not changed then return false end
  local presets = {}
  for i, key in ipairs(keys) do presets[i] = key end
  own(p).presets = presets
  return true
end

-- moves tx to the next preset after the current one that tx policy allows
function channels.cycle(p)
  if not is_player(p) then return false, "invalid-player" end
  local reason = lock_refusal(p, "tx")
  if reason then return false, reason end
  local r = record(p)
  local n = #r.presets
  if n == 0 then return false, "no-presets" end
  local at = 0
  for i, key in ipairs(r.presets) do
    if key == r.tx then at = i end
  end
  for step = 1, n do
    local key = r.presets[(at + step - 1) % n + 1]
    if key ~= r.tx and not tx_refusal(p, key) then return channels.set_tx(p, key) end
  end
  return false, "no-allowed-preset"
end

-- the first contradiction in the whole policy, checked in sorted order
local function contradiction()
  local policy = storage.radio_policy
  local admin_channel = policy.admin_channel
  if admin_channel and policy.banned[admin_channel] then return "mandatory" end
  for _, key in ipairs(sorted_keys(policy.banned)) do
    if policy.pinned[key] then return "ban-pin" end
  end
  for _, p in ipairs(sorted_keys(policy.deny)) do
    if admin_channel and policy.deny[p][admin_channel] then return "mandatory" end
  end
  for _, p in ipairs(sorted_keys(policy.confine)) do
    local deny = policy.deny[p] or {}
    local keys = sorted_keys(policy.confine[p])
    if #keys == 0 then return "confine-empty" end
    for _, key in ipairs(keys) do
      if policy.banned[key] then return "confine-banned" end
      if deny[key] == "both" then return "confine-denied" end
    end
  end
  for _, p in ipairs(sorted_keys(policy.lock)) do
    if policy.lock[p] ~= "rx" and tx_refusal(p, record(p).tx) then return "lock-forbidden" end
  end
  return nil
end

local function same(a, b)
  if type(a) ~= "table" or type(b) ~= "table" then return a == b end
  for k in pairs(a) do if b[k] == nil then return false end end
  for k in pairs(b) do if a[k] == nil then return false end end
  return true
end

local function admin_refusal(admin_index)
  if admin_index == 0 then return nil end
  local admin = type(admin_index) == "number" and game.get_player(admin_index)
  return not (admin and admin.admin) and "not-admin" or nil
end

-- sets tbl[k] = value, keeping it only if the policy stays free of contradictions
local function apply(tbl, k, value)
  local old = tbl[k]
  if same(old, value) then return false end
  tbl[k] = value
  local reason = contradiction()
  if reason then
    tbl[k] = old
    return false, reason
  end
  return true
end

local function target_refusal(admin_index, p)
  if p == 0 then return admin_refusal(admin_index) or "server" end
  return admin_refusal(admin_index) or (not is_player(p) and "invalid-player") or nil
end

function channels.policy_mute(admin_index, p, on)
  local reason = target_refusal(admin_index, p)
  if reason then return false, reason end
  return apply(storage.radio_policy.muted, p, on and true or nil)
end

function channels.policy_publisher(admin_index, p, on)
  local reason = target_refusal(admin_index, p)
  if reason then return false, reason end
  return apply(storage.radio_policy.publishers, p, on and true or nil)
end

function channels.policy_lock(admin_index, p, mode)
  local reason = target_refusal(admin_index, p) or (mode ~= nil and not LOCK_MODES[mode] and "invalid-mode") or nil
  if reason then return false, reason end
  return apply(storage.radio_policy.lock, p, mode)
end

function channels.policy_ban(admin_index, key, on)
  local reason = admin_refusal(admin_index) or (not valid_key(key) and "invalid-channel") or nil
  if reason then return false, reason end
  return apply(storage.radio_policy.banned, key, on and true or nil)
end

function channels.policy_pin(admin_index, key, on)
  local reason = admin_refusal(admin_index) or (not valid_key(key) and "invalid-channel") or nil
  if reason then return false, reason end
  return apply(storage.radio_policy.pinned, key, on and true or nil)
end

-- designates key as the mandatory admin channel, or nil to clear it; "ptt/here" is
-- surface-relative and cannot be mandatory for everyone
function channels.policy_admin_channel(admin_index, key)
  local reason = admin_refusal(admin_index)
    or (key ~= nil and not valid_key(key) and "invalid-channel")
    or (key == channels.HERE and "here-mandatory") or nil
  if reason then return false, reason end
  return apply(storage.radio_policy, "admin_channel", key)
end

-- mode is "tx" (no speaking), "rx" (no listening), "both", or nil to clear
function channels.policy_deny(admin_index, p, key, mode)
  local reason = target_refusal(admin_index, p) or (not valid_key(key) and "invalid-channel")
    or (mode ~= nil and not DENY_MODES[mode] and "invalid-mode") or nil
  if reason then return false, reason end
  local deny = storage.radio_policy.deny
  local bucket = deny[p] or {}
  deny[p] = bucket
  local changed, refusal = apply(bucket, key, mode)
  if next(bucket) == nil then deny[p] = nil end
  return changed, refusal
end

-- keys is the permitted set as a list, or nil to lift the confinement
function channels.policy_confine(admin_index, p, keys)
  local reason = target_refusal(admin_index, p)
  if reason then return false, reason end
  local set
  if keys then
    set = {}
    for _, key in ipairs(keys) do
      if not valid_key(key) then return false, "invalid-channel" end
      set[key] = true
    end
  end
  return apply(storage.radio_policy.confine, p, set)
end

-- channels with at least one connected, radio-on member in the viewer's force scope,
-- plus pinned ones; a member has the key as tx or extra rx. «Здесь» counts only the
-- viewer's physical surface; banned channels are not listed. the admin channel is
-- always listed, flagged admin = true.
function channels.known(viewer_index)
  local viewer = viewer_index ~= 0 and game.get_player(viewer_index) or nil
  local force_scope = viewer and settings.global["ptt-radio-scope"].value == "force"
  local policy = storage.radio_policy
  local counts = {}
  for _, member in pairs(game.connected_players) do
    local r = record(member.index)
    if not r.off and not (force_scope and member.force.index ~= viewer.force.index) then
      local keys = { [r.tx] = true }
      for key in pairs(r.rx) do keys[key] = true end
      for key in pairs(keys) do
        local here_ok = key ~= channels.HERE
          or (viewer ~= nil and member.physical_surface.index == viewer.physical_surface.index)
        if here_ok and not policy.banned[key] then
          counts[key] = (counts[key] or 0) + 1
        end
      end
    end
  end
  for key in pairs(policy.pinned) do counts[key] = counts[key] or 0 end
  if policy.admin_channel then counts[policy.admin_channel] = counts[policy.admin_channel] or 0 end
  local out = {}
  for _, key in ipairs(sorted_keys(counts)) do
    out[#out + 1] = { key = key, count = counts[key], admin = key == policy.admin_channel or nil }
  end
  return out
end

return channels

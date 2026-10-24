-- speaker hud: one row per active talker, stacked down from a fixed origin near the
-- left edge of gui.screen. rows appear on an accepted START, keep their place until
-- the talker stops and the rest close up behind them.
--
-- storage.hud.rows is an array in activation order, so the array index is the
-- activation ordinal and no pairs order ever reaches the screen. control.lua sets
-- hud.visible from channel eligibility and accepted per-listener talk membership.
-- the echo marker reads control.lua's storage.echo_sources.

local hud = {}
local channels = require("channels")

hud.ECHO_COLOR = { r = 1, g = 0.25, b = 0.25, a = 1 }

local ECHO_GLYPH = "[img=item/programmable-speaker] "
local ROOT = "ptt_radio_hud"
local ROW = "ptt_radio_hud_"
local CONTENT, TAIL, BARS = "content", "tail", "bars"
local SERVER_COLOR = { r = 0.8, g = 0.8, b = 0.8, a = 1 }

-- sizes are gui units and scale with display_scale; the origin is a fraction of the
-- screen height in pixels, and rows never reach below BOTTOM
local ROW_WIDTH, ROW_HEIGHT, ROW_SPACING, LEFT_MARGIN = 240, 32, 4, 8
local ICON_SIZE, NAME_WIDTH, TAIL_WIDTH = 24, 136, 56
local TOP, BOTTOM = 0.4, 0.8
local SEGMENT_WIDTH, SEGMENT_HEIGHTS = 6, { 8, 12, 16 }
local UNKNOWN_COLOR = { r = 0.45, g = 0.45, b = 0.45 }
local LINK_TICKS = 300
local LEVEL_COLOR = {
  { r = 0.9, g = 0.25, b = 0.2 },
  { r = 0.95, g = 0.75, b = 0.2 },
  { r = 0.3, g = 0.85, b = 0.3 },
}

function hud.init()
  storage.hud = storage.hud or { rows = {}, links = {} }
  if not storage.hud.next_expiry and next(storage.hud.links) then storage.hud.next_expiry = game.tick end
end

local function find(sender)
  for i, row in ipairs(storage.hud.rows) do
    if row.sender == sender then return i end
  end
end

local function channel_sprite(key)
  return channels.sprite(key)
end

local function channel_tooltip(key)
  if key == channels.ANY then return { "ptt-radio-settings.name-any" } end
  if key == channels.HERE then return { "ptt-radio-settings.name-here" } end
  return key
end

local function paint(bars, link)
  local level = link and link.level or 0
  for i, segment in ipairs(bars.children) do
    if level == 0 then
      segment.value, segment.style.color = 1, UNKNOWN_COLOR
    else
      segment.value, segment.style.color = i <= level and 1 or 0, LEVEL_COLOR[level]
    end
  end
  bars.tooltip = link and { level == 0 and "ptt-radio.hud-link-unknown-details" or "ptt-radio.hud-link", { "ptt-radio.hud-route-" .. link.route }, { "ptt-radio.hud-delay-" .. link.delay_class } }
    or { "ptt-radio.hud-link-unknown" }
end

-- spacing is a flow style attribute, so a row is a frame around one horizontal flow
local function row_content(root, name)
  local frame = root.add({ type = "frame", name = name, style = "tooltip_frame" })
  frame.style.size = { ROW_WIDTH, ROW_HEIGHT }
  frame.style.padding = { 4, 6 }
  local content = frame.add({ type = "flow", name = CONTENT, direction = "horizontal" })
  content.style.horizontal_spacing = 6
  content.style.vertical_align = "center"
  return content
end

local function add_row(root, viewer, row)
  local sender = row.sender
  local line = row_content(root, ROW .. sender)
  local icon = line.add({ type = "sprite", sprite = channel_sprite(row.channel), tooltip = channel_tooltip(row.channel) })
  icon.style.size = ICON_SIZE
  icon.style.stretch_image_to_widget_size = true
  local speaker = sender ~= 0 and game.get_player(sender) or nil
  local name = line.add({ type = "label", caption = speaker and speaker.name or { "ptt-radio.server" } })
  name.style.font = "default-bold"
  name.style.width = NAME_WIDTH
  if speaker then
    local c = speaker.chat_color
    name.style.font_color = { r = c.r, g = c.g, b = c.b, a = 1 }
  else
    name.style.font_color = SERVER_COLOR
  end
  local tail = line.add({ type = "flow", name = TAIL, direction = "horizontal" })
  tail.style.width = TAIL_WIDTH
  tail.style.horizontal_align = "right"
  tail.style.vertical_align = "center"
  if storage.echo_sources[sender] then
    local echo = tail.add({ type = "label", caption = { "", ECHO_GLYPH, { "ptt-radio.hud-echo" } }, tooltip = { "ptt-radio.echo-tooltip" } })
    echo.style.font = "default-bold"
    echo.style.font_color = hud.ECHO_COLOR
    return
  end
  local bars = tail.add({ type = "flow", name = BARS, direction = "horizontal" })
  bars.style.horizontal_spacing = 2
  bars.style.vertical_align = "bottom"
  for _, height in ipairs(SEGMENT_HEIGHTS) do
    local segment = bars.add({ type = "progressbar" })
    segment.style.width = SEGMENT_WIDTH
    segment.style.height = height
    segment.style.bar_width = height
  end
  local links = storage.hud.links[viewer.index]
  paint(bars, links and links[sender])
end

-- rebuilds one viewer's stack; capacity depends on the screen, so this is also the reflow
function hud.render(viewer)
  for sender in pairs(storage.hud.links[viewer.index] or {}) do
    if not hud.visible(sender, viewer.index) then storage.hud.links[viewer.index][sender] = nil end
  end
  local screen = viewer.gui.screen
  if screen[ROOT] then screen[ROOT].destroy() end
  local shown = {}
  for _, row in ipairs(storage.hud.rows) do
    if hud.visible(row.sender, viewer.index) then shown[#shown + 1] = row end
  end
  if #shown == 0 then return end
  local resolution, scale = viewer.display_resolution, viewer.display_scale
  local top = math.floor(resolution.height * TOP)
  local pitch = (ROW_HEIGHT + ROW_SPACING) * scale
  local capacity = math.max(2, math.floor((resolution.height * BOTTOM - top + ROW_SPACING * scale) / pitch))
  local root = screen.add({ type = "flow", name = ROOT, direction = "vertical" })
  root.style.vertical_spacing = ROW_SPACING
  root.location = { x = math.floor(LEFT_MARGIN * scale), y = top }
  -- an overflowing stack keeps the earliest talkers and turns the last slot into «ещё N»
  local count = #shown > capacity and capacity - 1 or #shown
  for i = 1, count do add_row(root, viewer, shown[i]) end
  if count < #shown then
    row_content(root, nil).add({ type = "label", caption = { "ptt-radio.hud-more", #shown - count } }).style.font = "default-bold"
  end
end

hud.reflow = hud.render

local function render_viewers_of(sender)
  for _, viewer in pairs(game.connected_players) do
    if hud.visible(sender, viewer.index) then hud.render(viewer) end
  end
end

-- re-renders every connected viewer that shows sender's row, e.g. on an echo or colour change
function hud.refresh(sender)
  if find(sender) then render_viewers_of(sender) end
end

-- a repeated start keeps the row's place and only takes the new channel;
-- restart marks a new talkspurt or generation: the row moves to the end and its link state resets
function hud.start(sender, channel_key, restart)
  local i = find(sender)
  if i and restart then
    table.remove(storage.hud.rows, i)
    for _, links in pairs(storage.hud.links) do links[sender] = nil end
    i = nil
  end
  if i then
    if storage.hud.rows[i].channel == channel_key then return end
    storage.hud.rows[i].channel = channel_key
    for _, viewer in pairs(game.connected_players) do
      if hud.visible(sender, viewer.index) then
        local root = viewer.gui.screen[ROOT]
        local row = root and root[ROW .. sender]
        local icon = row and row[CONTENT].children[1]
        if icon and icon.valid then
          icon.sprite, icon.tooltip = channel_sprite(channel_key), channel_tooltip(channel_key)
        else
          hud.render(viewer)
        end
      end
    end
    return
  else
    storage.hud.rows[#storage.hud.rows + 1] = { sender = sender, channel = channel_key }
  end
  render_viewers_of(sender)
end

function hud.stop(sender)
  local i = find(sender)
  if not i then return end
  table.remove(storage.hud.rows, i)
  for _, links in pairs(storage.hud.links) do links[sender] = nil end
  render_viewers_of(sender)
end

local function repaint_link(viewer, sender, link)
  local root = viewer.gui.screen[ROOT]
  local row = root and root[ROW .. sender]
  local bars = row and row[CONTENT][TAIL][BARS]
  if bars then paint(bars, link) end
end

-- level 0 remains unknown even when the selected route is known.
function hud.set_link(viewer, sender, level, route, delay_class)
  local links = storage.hud.links[viewer.index] or {}
  storage.hud.links[viewer.index] = links
  links[sender] = { level = level, route = route, delay_class = delay_class, tick = game.tick }
  local expires = game.tick + LINK_TICKS
  if not storage.hud.next_expiry or expires < storage.hud.next_expiry then storage.hud.next_expiry = expires end
  repaint_link(viewer, sender, links[sender])
end

function hud.clear_links(viewer)
  local links = storage.hud.links[viewer.index]
  storage.hud.links[viewer.index] = nil
  for sender in pairs(links or {}) do repaint_link(viewer, sender, nil) end
end

function hud.expire(tick)
  if not storage.hud.next_expiry or tick < storage.hud.next_expiry then return end
  local next_expiry
  for index, links in pairs(storage.hud.links) do
    local viewer = game.get_player(index)
    for sender, link in pairs(links) do
      if not link.tick or tick - link.tick >= LINK_TICKS then
        links[sender] = nil
        if viewer and viewer.connected then repaint_link(viewer, sender, nil) end
      else
        local expires = link.tick + LINK_TICKS
        if not next_expiry or expires < next_expiry then next_expiry = expires end
      end
    end
  end
  storage.hud.next_expiry = next_expiry
end

return hud

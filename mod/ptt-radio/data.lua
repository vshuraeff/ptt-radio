-- ALT + V is unbound in vanilla 2.1 (ALT + T, the v0.1 default, opens the technology screen)
data:extend({
  {
    type = "custom-input",
    name = "ptt-radio-toggle-mic",
    key_sequence = "ALT + V",
    consuming = "none",
    order = "a",
  },
  {
    type = "shortcut",
    name = "ptt-radio-open-mic",
    action = "lua",
    toggleable = true,
    associated_control_input = "ptt-radio-toggle-mic",
    icon = "__ptt-radio__/graphics/ptt-radio-x56.png",
    icon_size = 56,
    small_icon = "__ptt-radio__/graphics/ptt-radio-x24.png",
    small_icon_size = 24,
    order = "z[ptt-radio]",
  },
})

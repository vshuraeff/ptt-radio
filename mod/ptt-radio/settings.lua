data:extend({
  {
    type = "int-setting",
    name = "ptt-radio-companion-port",
    setting_type = "runtime-per-user",
    default_value = 34555,
    minimum_value = 1024,
    maximum_value = 65535,
    order = "a",
  },
  {
    type = "bool-setting",
    name = "ptt-radio-echo",
    setting_type = "runtime-per-user",
    default_value = false,
    order = "b",
  },
  {
    type = "bool-setting",
    name = "ptt-radio-half-duplex",
    setting_type = "runtime-per-user",
    default_value = true,
    order = "c",
  },
  {
    type = "string-setting",
    name = "ptt-radio-scope",
    setting_type = "runtime-global",
    default_value = "all",
    allowed_values = { "all", "force" },
    order = "a",
  },
})

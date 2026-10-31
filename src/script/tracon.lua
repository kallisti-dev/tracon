-- The module `require("tracon")` gives a configuration file. `native` is the
-- daemon's side: it keeps what `setup` collects, and holds the built-in
-- status function and the label tables.
local native = ...

local tracon = {}

-- The keys `setup` takes, each with the types it accepts.
local keys = {
  order = { "string", "table" },
  recent_first = { "string", "table" },
  preview_interval = { "number" },
  format = { "string" },
  labels = { "table", "function" },
  icons = { "table", "function" },
  styles = { "table", "function" },
  status = { "function" },
  summary = { "table" },
}
local summary_keys = {
  format = { "string" },
  separator = { "string" },
}

-- Reject a key `allowed` doesn't name and a value of another type, blaming
-- the caller `level` frames up.
local function check(options, allowed, name, level)
  if type(options) ~= "table" then
    error(("%s takes a table, not %s"):format(name, type(options)), level)
  end
  for key, value in pairs(options) do
    local types = type(key) == "string" and allowed[key]
    if not types then
      error(("%s has no key %s"):format(name, tostring(key)), level)
    end
    local given = type(value)
    local accepted = false
    for _, wanted in ipairs(types) do
      if given == wanted then
        accepted = true
      end
    end
    if not accepted then
      error(("%s.%s takes a %s, not %s"):format(name, key, table.concat(types, " or "), given), level)
    end
  end
end

-- Apply settings over the defaults, and what `setup` was given before:
-- `order`, `recent_first`, `preview_interval` (milliseconds), `format`,
-- `labels`, `icons` and `styles` (a table of status to text, or a function
-- of a status), `status` (a function of a pane), and `summary` with its
-- `format` and `separator`.
function tracon.setup(options)
  check(options, keys, "setup", 3)
  if options.summary ~= nil then
    check(options.summary, summary_keys, "setup.summary", 3)
  end
  local ok, message = native.configure(options)
  if not ok then
    error(message, 2)
  end
end

-- The built-in status function: the status of a pane, or nil.
tracon.status = native.status
-- The built-in label, icon and style of a status, from the tables as set up
-- so far; nil when they have none.
tracon.label = native.label
tracon.icon = native.icon
tracon.style = native.style

return tracon

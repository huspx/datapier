local bootstrap = require("./lib/bootstrap")
local window_bridge = require("./lib/window_bridge")

window_bridge.start()

tern.command({
  id = "new-datapier",
  title = "New Datapier block",
  icon = "database",
  run = function(cx)
    bootstrap.start(cx)
  end,
})

local bootstrap = require("./lib/bootstrap")

tern.command({
  id = "new-datapier",
  title = "New Datapier block",
  icon = "database",
  run = function(cx)
    bootstrap.start(cx)
  end,
})

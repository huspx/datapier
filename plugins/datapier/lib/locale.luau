-- 与已绑定的工作台使用同一种语言；首次绑定之前使用英文。
local M = {}
local english = true

function M.observe(resolved_english)
  english = resolved_english
end

function M.text(chinese, translated)
  return if english then translated else chinese
end

return M

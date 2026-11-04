"""读取并同步发布工具使用的产品版本。"""

import argparse
import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
PLUGIN_VALUE = re.compile(
  rf'(?m)^[ \t]*version[ \t]*=[ \t]*"({VERSION_PATTERN})"[ \t]*(?:\#[^\r\n]*)?(?=\r?$)'
)
PLUGIN_KEY = re.compile(r"(?m)^[ \t]*version[ \t]*=")
FIRST_TABLE = re.compile(r"(?m)^[ \t]*\[")


def product_version():
  try:
    with (ROOT / "Cargo.toml").open("rb") as manifest:
      version = tomllib.load(manifest)["workspace"]["package"]["version"]
    if not isinstance(version, str) or not re.fullmatch(VERSION_PATTERN, version):
      raise ValueError("invalid version")
  except (OSError, KeyError, TypeError, ValueError) as error:
    raise ValueError("Invalid workspace.package.version in Cargo.toml") from error
  return version


def resolve_product_version(requested=None):
  version = product_version()
  if requested is not None and requested != version:
    raise ValueError(
      f"Requested version {requested} does not match workspace version {version}"
    )
  return version


def set_plugin_version(path, version, check=False):
  path = Path(path)
  # 按字节往返保留 CRLF 和版本值以外的所有字符。
  text = path.read_bytes().decode("utf-8")
  try:
    parsed = tomllib.loads(text)
  except tomllib.TOMLDecodeError as error:
    raise ValueError(f"Invalid or duplicate plugin version in {path}") from error
  table = FIRST_TABLE.search(text)
  top = text[: table.start()] if table else text
  keys = list(PLUGIN_KEY.finditer(top))
  values = list(PLUGIN_VALUE.finditer(top))
  if len(keys) != 1 or len(values) != 1 or parsed.get("version") != values[0].group(1):
    raise ValueError(f"Invalid or duplicate plugin version in {path}")
  value = values[0]
  if value.group(1) != version:
    if check:
      raise ValueError(
        f"Plugin version {value.group(1)} does not match workspace version {version} in {path}"
      )
    updated = text[: value.start(1)] + version + text[value.end(1) :]
    path.write_bytes(updated.encode("utf-8"))


def check_release():
  """只读核对交付版本与变更归档；线上发布状态仍由 GitHub Release 管理。"""
  version = product_version()
  set_plugin_version(ROOT / "plugins/datapier/plugin.toml", version, check=True)
  text = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
  text = re.sub(r"<!--.*?(?:-->|\Z)", "", text, flags=re.DOTALL)
  titles = []
  changed_sections = set()
  fence = None
  # 文档中的示例和注释不能冒充版本章节或实际变更条目。
  for line in text.splitlines():
    if fence is not None:
      if re.fullmatch(rf" {{0,3}}{re.escape(fence[0])}{{{len(fence)},}}[ \t]*", line):
        fence = None
      continue
    marker = re.match(r" {0,3}(`{3,}|~{3,})", line)
    if marker:
      fence = marker.group(1)
      continue
    heading = re.fullmatch(r"##[ \t]+(.+?)[ \t]*", line)
    if heading:
      titles.append(heading.group(1))
    elif titles and re.match(r"[-*+][ \t]+\S", line):
      changed_sections.add(len(titles) - 1)
  versions = [title for title in titles if re.fullmatch(VERSION_PATTERN, title)]
  if len(versions) != len(set(versions)):
    raise ValueError("Duplicate version sections in CHANGELOG.md")
  if not titles or titles[0] != version:
    raise ValueError(f"First CHANGELOG.md section must be ## {version}")
  if 0 not in changed_sections:
    raise ValueError(f"CHANGELOG.md ## {version} must contain change entries")
  return version


if __name__ == "__main__":
  parser = argparse.ArgumentParser(description=__doc__)
  actions = parser.add_mutually_exclusive_group()
  actions.add_argument(
    "--sync",
    action="store_true",
    help="sync the source plugin manifest to the workspace version",
  )
  actions.add_argument(
    "--check-release",
    action="store_true",
    help="check plugin and changelog without changing files",
  )
  args = parser.parse_args()
  version = check_release() if args.check_release else product_version()
  if args.sync:
    set_plugin_version(ROOT / "plugins/datapier/plugin.toml", version)
  print(version)

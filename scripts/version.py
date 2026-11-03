"""读取并同步发布工具使用的产品版本。"""

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


if __name__ == "__main__":
  print(product_version())

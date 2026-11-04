"""从六个原生产物生成发布校验清单和插件包。"""

import argparse
import hashlib
import os
import re
import shutil
import tempfile
import zipfile
from pathlib import Path

from version import ROOT, resolve_product_version, set_plugin_version

TARGETS = (
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--artifacts", default="dist")
  parser.add_argument("--version")
  parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"))
  args = parser.parse_args()
  version = resolve_product_version(args.version)
  if not args.repository or not re.fullmatch(
    r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository
  ):
    raise ValueError(
      "Set --repository or GITHUB_REPOSITORY to the actual GitHub owner/repository"
    )
  directory = Path(args.artifacts).resolve(strict=True)
  lines = []
  binary_lines = []
  for target in TARGETS:
    extension = "zip" if target.endswith("-windows-msvc") else "tar.gz"
    name = f"datapier-{version}-{target}.{extension}"
    artifact = directory / name
    if not artifact.is_file():
      raise FileNotFoundError(f"Missing release artifact: {name}")
    with artifact.open("rb") as source:
      digest = hashlib.file_digest(source, "sha256").hexdigest()
    lines.append(f"{digest}  {name}")
    sidecar = directory / f"datapier-{version}-{target}.binary-sha256"
    if not sidecar.is_file():
      raise FileNotFoundError(f"Missing binary digest: {target}")
    digest = sidecar.read_text(encoding="utf-8").strip()
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
      raise ValueError(f"Invalid binary digest: {target}")
    binary_lines.append(f"{digest}  {target}")
  manifest = "\n".join(lines) + "\n"
  (directory / "SHA256SUMS").write_text(manifest, encoding="utf-8", newline="\n")
  with tempfile.TemporaryDirectory(prefix="datapier-release-") as temporary:
    plugin = Path(temporary) / "datapier"
    shutil.copytree(ROOT / "plugins/datapier", plugin)
    set_plugin_version(plugin / "plugin.toml", version)
    shutil.copy2(ROOT / "LICENSE", plugin / "LICENSE")
    (plugin / "release-sha256.txt").write_text(manifest, encoding="utf-8", newline="\n")
    (plugin / "release-bin-sha256.txt").write_text(
      "\n".join(binary_lines) + "\n", encoding="utf-8", newline="\n"
    )
    (plugin / "release-url.txt").write_text(
      f"https://github.com/{args.repository}/releases/download/v{version}\n",
      encoding="utf-8",
      newline="\n",
    )
    with zipfile.ZipFile(
      directory / f"datapier-plugin-{version}.zip",
      "w",
      compression=zipfile.ZIP_DEFLATED,
    ) as archive:
      for path in sorted(plugin.rglob("*")):
        if path.is_file():
          archive.write(path, path.relative_to(plugin.parent))


if __name__ == "__main__":
  main()

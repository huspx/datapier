"""从六个原生产物生成带校验清单的插件包。"""

import argparse
import hashlib
import os
import re
import shutil
import stat
import tarfile
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


def binary_digest(artifact, windows):
  """只接受包内唯一的固定名称普通文件，并流式计算其摘要。"""
  expected = "datapier.exe" if windows else "datapier"
  if windows:
    with zipfile.ZipFile(artifact) as archive:
      members = archive.infolist()
      if len(members) != 1 or members[0].filename != expected:
        raise ValueError(f"Expected one {expected} member in {artifact.name}")
      member = members[0]
      mode = member.external_attr >> 16
      if member.is_dir() or stat.S_IFMT(mode) not in (0, stat.S_IFREG):
        raise ValueError(f"Expected a regular {expected} file in {artifact.name}")
      with archive.open(member) as source:
        return hashlib.file_digest(source, "sha256").hexdigest()
  with tarfile.open(artifact, "r:gz") as archive:
    members = archive.getmembers()
    if len(members) != 1 or members[0].name != expected or not members[0].isfile():
      raise ValueError(f"Expected one regular {expected} file in {artifact.name}")
    with archive.extractfile(members[0]) as source:
      return hashlib.file_digest(source, "sha256").hexdigest()


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
    windows = target.endswith("-windows-msvc")
    extension = "zip" if windows else "tar.gz"
    name = f"datapier-{version}-{target}.{extension}"
    artifact = directory / name
    if not artifact.is_file():
      raise FileNotFoundError(f"Missing release artifact: {name}")
    with artifact.open("rb") as source:
      digest = hashlib.file_digest(source, "sha256").hexdigest()
    lines.append(f"{digest}  {name}")
    binary_lines.append(f"{binary_digest(artifact, windows)}  {target}")
  manifest = "\n".join(lines) + "\n"
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

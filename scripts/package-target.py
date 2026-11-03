"""打包在对应原生运行环境中构建并验证的程序。"""

import argparse
import hashlib
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path

from version import ROOT, resolve_product_version

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
  parser.add_argument("--target", required=True, choices=TARGETS)
  parser.add_argument("--version")
  parser.add_argument("--output", default="dist")
  args = parser.parse_args()
  version = resolve_product_version(args.version)
  output = ROOT / args.output
  windows = args.target.endswith("-windows-msvc")
  name = "datapier.exe" if windows else "datapier"
  binary = ROOT / "target" / args.target / "release" / name
  if not binary.is_file():
    raise FileNotFoundError(f"Missing built binary: {binary}")
  result = subprocess.run(
    [str(binary), "--version"], capture_output=True, text=True, check=False
  )
  if result.returncode != 0 or result.stdout.strip() != f"datapier {version}":
    raise ValueError(f"Built binary version does not match workspace version {version}")
  output.mkdir(parents=True, exist_ok=True)
  stem = f"datapier-{version}-{args.target}"
  with tempfile.TemporaryDirectory(prefix="datapier-package-") as temporary:
    packaged = Path(temporary) / name
    shutil.copy2(binary, packaged)
    if windows:
      with zipfile.ZipFile(
        output / f"{stem}.zip", "w", compression=zipfile.ZIP_DEFLATED
      ) as archive:
        archive.write(packaged, name)
    else:
      with tarfile.open(
        output / f"{stem}.tar.gz", "w:gz", format=tarfile.PAX_FORMAT
      ) as archive:
        archive.add(packaged, arcname=name, recursive=False)
    with packaged.open("rb") as source:
      digest = hashlib.file_digest(source, "sha256").hexdigest()
    (output / f"{stem}.binary-sha256").write_text(
      digest + "\n", encoding="utf-8", newline="\n"
    )


if __name__ == "__main__":
  main()

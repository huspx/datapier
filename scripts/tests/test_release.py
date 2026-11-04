"""验证发布版本和产物汇总对使用者可见的行为。"""

import hashlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import tarfile
import unittest
import zipfile
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
TARGETS = (
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
)


class ReleaseScriptsTest(unittest.TestCase):
  def setUp(self):
    self.temp = tempfile.TemporaryDirectory()
    self.addCleanup(self.temp.cleanup)
    self.root = Path(self.temp.name)
    scripts = self.root / "scripts"
    scripts.mkdir()
    for name in (
      "version.py",
      "package-target.py",
      "assemble-release.py",
    ):
      shutil.copy2(SCRIPTS / name, scripts / name)
    (self.root / "Cargo.toml").write_text(
      '[workspace.package]\nversion = "1.2.3"\n', encoding="utf-8"
    )
    (self.root / "CHANGELOG.md").write_text(
      "## 1.2.3\n\n- 完成查询能力。\n", encoding="utf-8"
    )
    self.plugin = self.root / "plugins/datapier"
    self.plugin.mkdir(parents=True)
    self.manifest = self.plugin / "plugin.toml"
    self.manifest.write_bytes(
      b'version = "1.0.0" # keep\r\n[other]\r\nversion = "9.0.0"\r\n'
    )
    (self.root / "LICENSE").write_text("license\n", encoding="utf-8")
    self.dist = self.root / "dist"
    self.dist.mkdir()

  def run_script(self, name, *arguments):
    env = os.environ.copy()
    env.pop("GITHUB_REPOSITORY", None)
    return subprocess.run(
      [sys.executable, str(self.root / "scripts" / name), *arguments],
      cwd=self.root,
      env=env,
      text=True,
      capture_output=True,
      check=False,
    )

  def artifacts(self):
    for target in TARGETS:
      windows = target.endswith("-windows-msvc")
      extension = "zip" if windows else "tar.gz"
      path = self.dist / f"datapier-1.2.3-{target}.{extension}"
      payload = target.encode()
      if windows:
        with zipfile.ZipFile(path, "w") as archive:
          archive.writestr("datapier.exe", payload)
      else:
        with tarfile.open(path, "w:gz") as archive:
          member = tarfile.TarInfo("datapier")
          member.size = len(payload)
          archive.addfile(member, io.BytesIO(payload))

  def test_check_is_read_only_and_sync_preserves_format(self):
    before = self.manifest.read_bytes()
    read = self.run_script("version.py")
    self.assertEqual(read.returncode, 0, read.stderr)
    self.assertEqual(read.stdout.strip(), "1.2.3")
    self.assertEqual(self.manifest.read_bytes(), before)
    self.assertNotEqual(self.run_script("version.py", "--check-release").returncode, 0)
    self.assertEqual(self.manifest.read_bytes(), before)
    self.assertEqual(self.run_script("version.py", "--sync").returncode, 0)
    self.assertEqual(self.manifest.read_bytes(), before.replace(b'"1.0.0"', b'"1.2.3"'))
    self.assertEqual(self.run_script("version.py", "--check-release").returncode, 0)
    synced = self.manifest.read_bytes()
    conflict = self.run_script("version.py", "--sync", "--check-release")
    self.assertNotEqual(conflict.returncode, 0)
    self.assertEqual(self.manifest.read_bytes(), synced)

  def test_release_check_requires_matching_plugin_and_does_not_change_files(self):
    changelog = self.root / "CHANGELOG.md"
    changelog.write_text(
      "# 变更记录\n\n## 1.2.3\n\n- 支持受控查询。\n\n"
      "```markdown\n## 1.2.3\n- 示例章节。\n```\n"
      "<!--\n## 1.2.3\n- 注释章节。\n-->\n\n## 1.2.2\n\n- 历史变更。\n",
      encoding="utf-8",
    )
    before_manifest = self.manifest.read_bytes()
    mismatch = self.run_script("version.py", "--check-release")
    self.assertNotEqual(mismatch.returncode, 0)
    self.assertIn("does not match workspace version", mismatch.stderr)
    self.assertEqual(self.manifest.read_bytes(), before_manifest)
    self.assertEqual(self.run_script("version.py", "--sync").returncode, 0)
    paths = (self.manifest, changelog, self.root / "Cargo.toml")
    before = {path: path.read_bytes() for path in paths}
    result = self.run_script("version.py", "--check-release")
    self.assertEqual(result.returncode, 0, result.stderr)
    self.assertEqual(result.stdout.strip(), "1.2.3")
    self.assertEqual({path: path.read_bytes() for path in paths}, before)

  def test_release_check_rejects_missing_duplicate_or_unarchived_changes(self):
    self.assertEqual(self.run_script("version.py", "--sync").returncode, 0)
    changelog = self.root / "CHANGELOG.md"
    for text in (
      "## 未发布（Unreleased）\n\n- 新功能。\n",
      "## 1.2.4\n\n- 其他版本。\n\n## 1.2.3\n\n- 当前变更。\n",
      "## 1.2.3\n\n- 变更。\n\n## 1.2.3\n\n- 重复章节。\n",
      "## 1.2.3（已发布）\n\n- 变更。\n",
      "## 1.2.3\n\n### 功能\n\n<!-- - 仅注释。 -->\n```\n- 仅示例。\n```\n",
      "```markdown\n## 1.2.3\n- 示例不能冒充版本。\n```\n",
    ):
      with self.subTest(changelog=text):
        changelog.write_text(text, encoding="utf-8")
        before = changelog.read_bytes()
        result = self.run_script("version.py", "--check-release")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CHANGELOG.md", result.stderr)
        self.assertEqual(changelog.read_bytes(), before)
    changelog.unlink()
    missing = self.run_script("version.py", "--check-release")
    self.assertNotEqual(missing.returncode, 0)
    self.assertIn("CHANGELOG.md", missing.stderr)

  def test_invalid_or_duplicate_versions_are_rejected(self):
    for data in (
      b'version = "01.2.3"\n',
      b'version = "1.2.3"\nversion = "1.2.3"\n',
    ):
      with self.subTest(data=data):
        self.manifest.write_bytes(data)
        self.assertNotEqual(self.run_script("version.py", "--sync").returncode, 0)
        self.assertEqual(self.manifest.read_bytes(), data)
    (self.root / "Cargo.toml").write_text(
      '[workspace.package]\nversion = "01.2.3"\n', encoding="utf-8"
    )
    self.assertNotEqual(self.run_script("version.py").returncode, 0)

  def test_package_rejects_missing_binary_and_wrong_explicit_version(self):
    result = self.run_script("package-target.py", "--target", TARGETS[0])
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("Missing built binary", result.stderr)
    result = self.run_script(
      "package-target.py", "--target", TARGETS[0], "--version", "1.2.4"
    )
    self.assertNotEqual(result.returncode, 0)
    self.assertIn("does not match workspace version", result.stderr)
    self.assertEqual(list(self.dist.iterdir()), [])

  @unittest.skipIf(os.name == "nt", "fixture executable requires a POSIX shell")
  def test_package_produces_only_archive(self):
    binary = self.root / "target" / TARGETS[0] / "release" / "datapier"
    binary.parent.mkdir(parents=True)
    binary.write_text('#!/bin/sh\necho "datapier 1.2.3"\n', encoding="utf-8")
    binary.chmod(0o755)
    result = self.run_script("package-target.py", "--target", TARGETS[0])
    self.assertEqual(result.returncode, 0, result.stderr)
    archive_path = self.dist / f"datapier-1.2.3-{TARGETS[0]}.tar.gz"
    self.assertEqual(list(self.dist.iterdir()), [archive_path])
    with tarfile.open(archive_path, "r:gz") as archive:
      self.assertEqual(archive.getnames(), ["datapier"])
      self.assertEqual(archive.extractfile("datapier").read(), binary.read_bytes())

  def test_assembly_rejects_missing_archive_before_writing(self):
    self.artifacts()
    missing = f"datapier-1.2.3-{TARGETS[-1]}.zip"
    (self.dist / missing).unlink()
    result = self.run_script("assemble-release.py", "--repository", "owner/repo")
    self.assertNotEqual(result.returncode, 0)
    self.assertIn(missing, result.stderr)
    self.assertFalse((self.dist / "datapier-plugin-1.2.3.zip").exists())

  def test_assembly_rejects_invalid_archive_members_before_writing(self):
    self.artifacts()
    cases = ("wrong-name", "duplicate", "symlink", "invalid-format")
    for target in (TARGETS[0], TARGETS[-1]):
      windows = target.endswith("-windows-msvc")
      extension = "zip" if windows else "tar.gz"
      path = self.dist / f"datapier-1.2.3-{target}.{extension}"
      original = path.read_bytes()
      for case in cases:
        with self.subTest(target=target, case=case):
          if case == "invalid-format":
            path.write_bytes(b"not an archive")
          elif windows:
            with zipfile.ZipFile(path, "w") as archive:
              member = zipfile.ZipInfo(
                "wrong" if case == "wrong-name" else "datapier.exe"
              )
              if case == "symlink":
                member.create_system = 3
                member.external_attr = 0o120777 << 16
              archive.writestr(member, b"binary")
              if case == "duplicate":
                archive.writestr("datapier.exe", b"binary")
          else:
            with tarfile.open(path, "w:gz") as archive:
              member = tarfile.TarInfo("wrong" if case == "wrong-name" else "datapier")
              if case == "symlink":
                member.type = tarfile.SYMTYPE
                member.linkname = "elsewhere"
                archive.addfile(member)
              else:
                member.size = 6
                archive.addfile(member, io.BytesIO(b"binary"))
              if case == "duplicate":
                member = tarfile.TarInfo("datapier")
                archive.addfile(member, io.BytesIO())
          result = self.run_script("assemble-release.py", "--repository", "owner/repo")
          self.assertNotEqual(result.returncode, 0)
          self.assertFalse((self.dist / "datapier-plugin-1.2.3.zip").exists())
          path.write_bytes(original)

  def test_assembly_contains_release_metadata_without_mutating_source(self):
    self.artifacts()
    before = {
      path.relative_to(self.plugin): path.read_bytes()
      for path in self.plugin.rglob("*")
      if path.is_file()
    }
    result = self.run_script("assemble-release.py", "--repository", "owner/repo")
    self.assertEqual(result.returncode, 0, result.stderr)
    self.assertEqual(
      {
        path.relative_to(self.plugin): path.read_bytes()
        for path in self.plugin.rglob("*")
        if path.is_file()
      },
      before,
    )
    self.assertEqual(len(list(self.dist.iterdir())), 7)
    manifest_before = before[Path("plugin.toml")]
    with zipfile.ZipFile(self.dist / "datapier-plugin-1.2.3.zip") as archive:
      self.assertTrue(all(name.startswith("datapier/") for name in archive.namelist()))
      self.assertEqual(
        archive.read("datapier/plugin.toml"),
        manifest_before.replace(b'"1.0.0"', b'"1.2.3"'),
      )
      self.assertEqual(archive.read("datapier/LICENSE"), b"license\n")
      sums = archive.read("datapier/release-sha256.txt").decode().splitlines()
      binary_sums = (
        archive.read("datapier/release-bin-sha256.txt").decode().splitlines()
      )
      self.assertEqual(len(sums), 6)
      self.assertEqual(len(binary_sums), 6)
      for target, line, binary_line in zip(TARGETS, sums, binary_sums):
        digest, name = line.split("  ")
        binary_digest, binary_target = binary_line.split("  ")
        self.assertEqual(binary_target, target)
        extension = "zip" if target.endswith("-windows-msvc") else "tar.gz"
        self.assertEqual(name, f"datapier-1.2.3-{target}.{extension}")
        path = self.dist / name
        self.assertEqual(digest, hashlib.sha256(path.read_bytes()).hexdigest())
        if target.endswith("-windows-msvc"):
          with zipfile.ZipFile(path) as package:
            payload = package.read("datapier.exe")
        else:
          with tarfile.open(path, "r:gz") as package:
            payload = package.extractfile("datapier").read()
        self.assertEqual(binary_digest, hashlib.sha256(payload).hexdigest())
      self.assertEqual(
        archive.read("datapier/release-url.txt"),
        b"https://github.com/owner/repo/releases/download/v1.2.3\n",
      )


if __name__ == "__main__":
  unittest.main()

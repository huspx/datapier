"""通过隔离的 GitHub REST 服务验证草稿 Release CLI。"""

import copy
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tarfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit
import zipfile


SOURCE_ROOT = Path(__file__).resolve().parents[2]
VERSION = "1.2.3"
COMMIT = "a" * 40
OTHER_COMMIT = "b" * 40
TARGETS = (
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
)
TOKEN = "test-token"


def name(target):
  ext = "zip" if target.endswith("windows-msvc") else "tar.gz"
  return f"datapier-{VERSION}-{target}.{ext}"


class GitHubServer:
  """保留 Release 状态的 REST fixture，资源字节与 JSON 元数据分开存储。"""

  def __init__(self):
    self.releases = {}
    self.assets = {}
    self.next_release = 1
    self.next_asset = 1
    self.tag = None
    self.tag_objects = {}
    self.redirect_downloads = False
    self.redirect_authorizations = []
    self.reset_missing_patch_tag = False
    self.persisted_patch_overrides = {}
    fixture = self

    class Handler(BaseHTTPRequestHandler):
      protocol_version = "HTTP/1.1"

      def log_message(self, *args):
        pass

      def reply(self, status, value=None, headers=None):
        data = (
          b""
          if value is None
          else (value if isinstance(value, bytes) else json.dumps(value).encode())
        )
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.send_header(
          "Content-Type",
          "application/octet-stream"
          if isinstance(value, bytes)
          else "application/json",
        )
        for key, val in (headers or {}).items():
          self.send_header(key, val)
        self.end_headers()
        if data:
          self.wfile.write(data)

      def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

      def paginate(self, values):
        query = parse_qs(urlsplit(self.path).query)
        size = int(query.get("per_page", [30])[0])
        page = int(query.get("page", [1])[0])
        selected = values[(page - 1) * size : page * size]
        headers = {}
        if page * size < len(values):
          path = urlsplit(self.path).path
          headers["Link"] = (
            f'<{fixture.url}{path}?per_page={size}&page={page + 1}>; rel="next"'
          )
        self.reply(200, selected, headers)

      def dispatch(self):
        parsed = urlsplit(self.path)
        path = unquote(parsed.path)
        method = self.command
        if path.startswith("/redirect/"):
          fixture.redirect_authorizations.append(self.headers.get("Authorization"))
          asset = fixture.assets.get(int(path.rsplit("/", 1)[1]))
          return self.reply(200, asset["data"]) if asset else self.reply(404, {})
        if self.headers.get("Authorization") not in (
          f"Bearer {TOKEN}",
          f"token {TOKEN}",
        ):
          return self.reply(401, {"message": "Bad credentials"})
        base = "/repos/owner/repo"
        if path == base + "/releases":
          if method == "GET":
            return self.paginate(list(fixture.releases.values()))
          if method == "POST":
            release = json.loads(self.body())
            release["id"] = fixture.next_release
            fixture.next_release += 1
            release.setdefault("body", "")
            release.setdefault("draft", False)
            release["upload_url"] = (
              f"{fixture.url}/upload/{release['id']}{{?name,label}}"
            )
            fixture.releases[release["id"]] = release
            return self.reply(201, release)
        if path == base + "/git/ref/tags/v" + VERSION and method == "GET":
          if fixture.tag is None:
            return self.reply(404, {"message": "Not Found"})
          return self.reply(
            200, {"ref": f"refs/tags/v{VERSION}", "object": fixture.tag}
          )
        if path.startswith(base + "/git/tags/") and method == "GET":
          obj = fixture.tag_objects.get(path.rsplit("/", 1)[1])
          return self.reply(200, {"object": obj}) if obj else self.reply(404, {})
        if path.startswith("/upload/") and method == "POST":
          release_id = int(path.rsplit("/", 1)[1])
          name = parse_qs(parsed.query)["name"][0]
          data = self.body()
          if any(
            a["release_id"] == release_id and a["name"] == name
            for a in fixture.assets.values()
          ):
            return self.reply(422, {"message": "already_exists"})
          asset = fixture.add_asset(release_id, name, data)
          return self.reply(201, fixture.asset_json(asset))
        asset_base = base + "/releases/assets/"
        if path.startswith(asset_base):
          asset_id = int(path[len(asset_base) :])
          asset = fixture.assets.get(asset_id)
          if not asset:
            return self.reply(404, {})
          if method == "DELETE":
            del fixture.assets[asset_id]
            return self.reply(204)
          if method == "GET":
            if fixture.redirect_downloads:
              # 改变主机名即可验证跨源跳转时的凭据隔离。
              url = fixture.url.replace("127.0.0.1", "localhost")
              return self.reply(302, headers={"Location": f"{url}/redirect/{asset_id}"})
            return self.reply(200, asset["data"])
        release_base = base + "/releases/"
        if path.startswith(release_base):
          parts = path[len(release_base) :].split("/")
          try:
            release = fixture.releases[int(parts[0])]
          except ValueError, KeyError:
            return self.reply(404, {})
          if len(parts) == 2 and parts[1] == "assets" and method == "GET":
            return self.paginate(
              [
                fixture.asset_json(a)
                for a in fixture.assets.values()
                if a["release_id"] == release["id"]
              ]
            )
          if len(parts) == 1 and method == "GET":
            return self.reply(200, release)
          if len(parts) == 1 and method == "PATCH":
            payload = json.loads(self.body())
            release.update(payload)
            # 模拟未显式保留 tag 时的漂移，以及 PATCH 成功后读到不同状态。
            if fixture.reset_missing_patch_tag and "tag_name" not in payload:
              release["tag_name"] = "untagged-fixture"
            response = copy.deepcopy(release)
            release.update(fixture.persisted_patch_overrides)
            return self.reply(200, response)
        return self.reply(404, {"message": f"unknown fixture endpoint {method} {path}"})

      do_GET = dispatch
      do_POST = dispatch
      do_PATCH = dispatch
      do_DELETE = dispatch

    self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    self.server.daemon_threads = True
    self.url = f"http://127.0.0.1:{self.server.server_port}"
    self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
    self.thread.start()

  def close(self):
    self.server.shutdown()
    self.server.server_close()
    self.thread.join()

  def add_asset(self, release_id, name, data, digest=None):
    asset = {
      "id": self.next_asset,
      "release_id": release_id,
      "name": name,
      "state": "uploaded",
      "data": data,
      "digest": digest
      if digest is not None
      else "sha256:" + hashlib.sha256(data).hexdigest(),
    }
    self.next_asset += 1
    self.assets[asset["id"]] = asset
    return asset

  @staticmethod
  def asset_json(asset):
    return {
      key: value for key, value in asset.items() if key not in ("data", "release_id")
    }

  def by_name(self):
    return {asset["name"]: asset["data"] for asset in self.assets.values()}


class DraftReleaseTests(unittest.TestCase):
  def setUp(self):
    self.temporary = tempfile.TemporaryDirectory()
    self.addCleanup(self.temporary.cleanup)
    self.root = Path(self.temporary.name)
    (self.root / "scripts").mkdir()
    for name in ("version.py", "assemble-release.py", "draft_release.py"):
      shutil.copy2(SOURCE_ROOT / "scripts" / name, self.root / "scripts" / name)
    (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.3"\n')
    (self.root / "CHANGELOG.md").write_text("## 1.2.3\n\n- 完成查询能力。\n")
    plugin = self.root / "plugins" / "datapier"
    plugin.mkdir(parents=True)
    (plugin / "plugin.toml").write_text('name = "datapier"\nversion = "1.2.3"\n')
    (plugin / "content.txt").write_text("fixture plugin\n")
    (self.root / "LICENSE").write_text("fixture license\n")
    self.artifacts = self.root / "dist"
    self.artifacts.mkdir()
    self.output = self.root / "output"
    self.server = GitHubServer()
    self.addCleanup(self.server.close)

  def cli(self, command, *args, commit=COMMIT, run="100", attempt="1", success=True):
    env = os.environ.copy()
    env.update(
      {
        "GH_TOKEN": TOKEN,
        "GITHUB_TOKEN": TOKEN,
        "GITHUB_OUTPUT": str(self.output),
      }
    )
    proc = subprocess.run(
      [
        sys.executable,
        str(self.root / "scripts" / "draft_release.py"),
        command,
        "--repository",
        "owner/repo",
        "--commit",
        commit,
        "--run-id",
        run,
        "--attempt",
        attempt,
        "--api-url",
        self.server.url,
        "--artifacts",
        str(self.artifacts),
        *map(str, args),
      ],
      cwd=self.root,
      env=env,
      capture_output=True,
      text=True,
      timeout=30,
    )
    if success:
      self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
    else:
      self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
    self.assertNotIn(TOKEN, proc.stdout + proc.stderr)
    return proc

  def prepare(self):
    self.cli("prepare")
    self.assertEqual(len(self.server.releases), 1)
    return next(iter(self.server.releases.values()))

  def write_target(self, target):
    archive = name(target)
    binary = (f"fixture binary for {target}\n").encode()
    path = self.artifacts / archive
    if target.endswith("-windows-msvc"):
      with zipfile.ZipFile(path, "w") as package:
        package.writestr("datapier.exe", binary)
    else:
      with tarfile.open(path, "w:gz") as package:
        member = tarfile.TarInfo("datapier")
        member.size = len(binary)
        package.addfile(member, io.BytesIO(binary))
    return archive, path.read_bytes()

  def upload_all(self, release):
    expected = {}
    for target in TARGETS:
      archive, payload = self.write_target(target)
      self.cli("upload-target", "--release-id", release["id"], "--target", target)
      expected[target] = (archive, payload)
    return expected

  def test_prepare_paginates_releases_and_assets(self):
    for index in range(105):
      release_id = self.server.next_release
      self.server.next_release += 1
      self.server.releases[release_id] = {
        "id": release_id,
        "tag_name": f"v0.0.{index}",
        "draft": True,
        "target_commitish": COMMIT,
        "body": "other release",
        "upload_url": f"{self.server.url}/upload/{release_id}{{?name,label}}",
      }
    self.cli("prepare")
    release = next(
      r for r in self.server.releases.values() if r["tag_name"] == "v" + VERSION
    )
    for index in range(105):
      self.server.add_asset(release["id"], f"manual-{index}.txt", str(index).encode())
    self.server.add_asset(release["id"], name(TARGETS[0]), b"old")
    self.cli("prepare", run="101")
    self.assertEqual(len(self.server.releases), 106)
    self.assertEqual(len(self.server.assets), 105)
    self.assertNotIn(name(TARGETS[0]), self.server.by_name())
    self.assertEqual(self.server.by_name()["manual-104.txt"], b"104")

  def test_upload_rerun_replaces_only_its_own_assets(self):
    release = self.prepare()
    archive, payload = self.write_target(TARGETS[0])
    self.cli("upload-target", "--release-id", release["id"], "--target", TARGETS[0])
    self.server.add_asset(release["id"], "manual.txt", b"keep")
    (self.artifacts / archive).write_bytes(payload + b"updated")
    self.cli("upload-target", "--release-id", release["id"], "--target", TARGETS[0])
    self.assertEqual(
      self.server.by_name(),
      {
        archive: payload + b"updated",
        "manual.txt": b"keep",
      },
    )

  def test_prepare_creates_draft_and_outputs_identity(self):
    release = self.prepare()
    self.assertTrue(release["draft"])
    self.assertEqual(release["name"], "Datapier " + VERSION)
    self.assertEqual(release["tag_name"], "v" + VERSION)
    self.assertEqual(release["target_commitish"], COMMIT)
    self.assertIn(f"<!-- datapier-build:{COMMIT}:100:1 -->", release["body"])
    output = self.output.read_text().splitlines()
    self.assertIn(f"release-id={release['id']}", output)
    self.assertIn("tag=v" + VERSION, output)

  def test_refresh_preserves_manual_content_and_cleans_only_generated_assets(self):
    release = self.prepare()
    release["body"] = "Human release notes\n\n" + release["body"]
    for target in TARGETS:
      self.server.add_asset(release["id"], name(target), b"old")
    self.server.add_asset(release["id"], f"datapier-plugin-{VERSION}.zip", b"old")
    self.server.add_asset(release["id"], "handwritten.txt", b"keep")
    self.server.add_asset(
      release["id"], "datapier-9.9.9-x86_64-apple-darwin.tar.gz", b"other version"
    )
    self.server.reset_missing_patch_tag = True
    self.cli("prepare", run="101", attempt="2", commit=OTHER_COMMIT)
    self.assertEqual(release["name"], "Datapier " + VERSION)
    self.assertEqual(release["tag_name"], "v" + VERSION)
    self.assertEqual(release["target_commitish"], OTHER_COMMIT)
    self.assertIn("Human release notes", release["body"])
    self.assertIn(f"<!-- datapier-build:{OTHER_COMMIT}:101:2 -->", release["body"])
    self.assertNotIn(f"<!-- datapier-build:{COMMIT}:100:1 -->", release["body"])
    self.assertEqual(
      self.server.by_name(),
      {
        "handwritten.txt": b"keep",
        "datapier-9.9.9-x86_64-apple-darwin.tar.gz": b"other version",
      },
    )

  def test_prepare_reuses_named_untagged_draft(self):
    release = self.prepare()
    release_id = release["id"]
    release["tag_name"] = "untagged-previous"
    release["body"] = "Human release notes\n\n" + release["body"]
    self.server.add_asset(release_id, "manual.txt", b"keep")
    self.server.reset_missing_patch_tag = True
    self.cli("prepare", run="101", commit=OTHER_COMMIT)
    self.assertEqual(len(self.server.releases), 1)
    self.assertEqual(release["id"], release_id)
    self.assertEqual(release["name"], "Datapier " + VERSION)
    self.assertEqual(release["tag_name"], "v" + VERSION)
    self.assertEqual(release["target_commitish"], OTHER_COMMIT)
    self.assertIn(f"<!-- datapier-build:{OTHER_COMMIT}:101:1 -->", release["body"])
    self.assertIn("Human release notes", release["body"])
    self.assertEqual(self.server.by_name(), {"manual.txt": b"keep"})

  def test_prepare_rejects_multiple_name_or_tag_matches_without_mutation(self):
    for second_tag in ("v" + VERSION, "untagged-second"):
      with self.subTest(second_tag=second_tag):
        self.server.releases.clear()
        self.server.assets.clear()
        first = self.prepare()
        first["tag_name"] = "untagged-first"
        second = copy.deepcopy(first)
        second["id"] = self.server.next_release
        self.server.next_release += 1
        second["tag_name"] = second_tag
        self.server.releases[second["id"]] = second
        self.server.add_asset(first["id"], name(TARGETS[0]), b"keep first")
        self.server.add_asset(second["id"], "manual.txt", b"keep second")
        before = copy.deepcopy((self.server.releases, self.server.assets))
        self.cli("prepare", run="101", success=False)
        self.assertEqual((self.server.releases, self.server.assets), before)

  def test_prepare_rejects_unowned_or_conflicting_named_draft(self):
    release = self.prepare()
    original = copy.deepcopy(release)
    self.server.add_asset(release["id"], "manual.txt", b"keep")
    for field, value in (
      ("body", "Human release notes"),
      ("body", original["body"] + "\n" + original["body"]),
      ("tag_name", "v9.9.9"),
    ):
      with self.subTest(field=field, value=value):
        release.clear()
        release.update(original)
        release[field] = value
        before = copy.deepcopy((self.server.releases, self.server.assets))
        self.cli("prepare", run="101", success=False)
        self.assertEqual((self.server.releases, self.server.assets), before)

  def test_invalid_changelog_cannot_refresh_or_upload_to_draft(self):
    release = self.prepare()
    self.write_target(TARGETS[0])
    self.server.add_asset(release["id"], name(TARGETS[0]), b"keep")
    before = copy.deepcopy((self.server.releases, self.server.assets))
    (self.root / "CHANGELOG.md").write_text("## 未发布（Unreleased）\n\n- 尚未归档。\n")
    commands = (
      ("prepare",),
      ("upload-target", "--release-id", release["id"], "--target", TARGETS[0]),
      ("assemble", "--release-id", release["id"]),
    )
    for command in commands:
      with self.subTest(command=command[0]):
        result = self.cli(*command, success=False)
        self.assertIn("CHANGELOG.md", result.stderr)
        self.assertEqual((self.server.releases, self.server.assets), before)

  def test_published_release_is_not_modified(self):
    release = self.prepare()
    release["draft"] = False
    self.server.add_asset(release["id"], name(TARGETS[0]), b"keep")
    before = copy.deepcopy((self.server.releases, self.server.assets))
    self.cli("prepare", success=False)
    self.assertEqual((self.server.releases, self.server.assets), before)

  def test_existing_tag_at_other_commit_is_rejected(self):
    self.server.tag = {"type": "commit", "sha": OTHER_COMMIT}
    self.cli("prepare", success=False)
    self.assertEqual(self.server.releases, {})
    self.assertEqual(self.server.assets, {})

  def test_annotated_tag_at_other_commit_is_rejected(self):
    self.server.tag = {"type": "tag", "sha": "c" * 40}
    self.server.tag_objects["c" * 40] = {"type": "commit", "sha": OTHER_COMMIT}
    self.cli("prepare", success=False)
    self.assertEqual(self.server.releases, {})

  def test_upload_rejects_changed_run_or_commit_without_mutation(self):
    release = self.prepare()
    self.write_target(TARGETS[0])
    before = copy.deepcopy((self.server.releases, self.server.assets))
    for identity in ({"run": "101"}, {"attempt": "2"}, {"commit": OTHER_COMMIT}):
      with self.subTest(identity=identity):
        self.cli(
          "upload-target",
          "--release-id",
          release["id"],
          "--target",
          TARGETS[0],
          success=False,
          **identity,
        )
        self.assertEqual((self.server.releases, self.server.assets), before)

  def test_assemble_rejects_missing_target_without_final_assets(self):
    release = self.prepare()
    self.server.add_asset(release["id"], name(TARGETS[0]), b"archive")
    before = copy.deepcopy((self.server.releases, self.server.assets))
    self.cli("assemble", "--release-id", release["id"], success=False)
    self.assertEqual((self.server.releases, self.server.assets), before)
    self.assertFalse((self.artifacts / f"datapier-plugin-{VERSION}.zip").exists())

  def test_assemble_rejects_corrupt_github_digest_without_final_assets(self):
    release = self.prepare()
    self.upload_all(release)
    next(iter(self.server.assets.values()))["digest"] = "sha256:" + "0" * 64
    before = copy.deepcopy((self.server.releases, self.server.assets))
    self.cli("assemble", "--release-id", release["id"], success=False)
    self.assertEqual((self.server.releases, self.server.assets), before)
    self.assertFalse((self.artifacts / f"datapier-plugin-{VERSION}.zip").exists())

  def test_assemble_rejects_identity_drift_after_successful_final_patch(self):
    for field, value in (
      ("name", "Another release"),
      ("tag_name", "untagged-after-patch"),
      ("target_commitish", OTHER_COMMIT),
      ("body", "Replaced externally"),
      ("draft", False),
    ):
      with self.subTest(field=field):
        self.server.releases.clear()
        self.server.assets.clear()
        self.server.persisted_patch_overrides = {}
        release = self.prepare()
        self.upload_all(release)
        self.server.persisted_patch_overrides = {field: value}
        self.cli("assemble", "--release-id", release["id"], success=False)
        self.assertEqual(release[field], value)
        self.assertEqual(
          set(self.server.by_name()),
          {name(target) for target in TARGETS} | {f"datapier-plugin-{VERSION}.zip"},
        )

  def test_complete_release_has_valid_manifest_and_plugin_metadata(self):
    release = self.prepare()
    expected = self.upload_all(release)
    self.server.reset_missing_patch_tag = True
    next(iter(self.server.assets.values()))["digest"] = None
    # 删除本地归档，确保汇总实际下载草稿中的资源。
    for path in self.artifacts.iterdir():
      path.unlink()
    self.server.redirect_downloads = True
    self.cli("assemble", "--release-id", release["id"])
    self.assertTrue(release["draft"])
    self.assertTrue(self.server.redirect_authorizations)
    self.assertTrue(all(value is None for value in self.server.redirect_authorizations))
    assets = self.server.by_name()
    manifest = "".join(
      f"{hashlib.sha256(data).hexdigest()}  {name}\n"
      for name, data in expected.values()
    )
    self.assertEqual(
      set(assets),
      {name for name, _ in expected.values()} | {f"datapier-plugin-{VERSION}.zip"},
    )
    for name, data in expected.values():
      self.assertEqual(assets[name], data)
    with zipfile.ZipFile(
      io.BytesIO(assets[f"datapier-plugin-{VERSION}.zip"])
    ) as plugin:
      self.assertEqual(plugin.read("datapier/release-sha256.txt"), manifest.encode())
      binary_manifest = ""
      for target, (_, data) in expected.items():
        if target.endswith("-windows-msvc"):
          with zipfile.ZipFile(io.BytesIO(data)) as package:
            binary = package.read("datapier.exe")
        else:
          with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as package:
            binary = package.extractfile("datapier").read()
        binary_manifest += f"{hashlib.sha256(binary).hexdigest()}  {target}\n"
      self.assertEqual(
        plugin.read("datapier/release-bin-sha256.txt"), binary_manifest.encode()
      )
      self.assertEqual(
        plugin.read("datapier/release-url.txt"),
        f"https://github.com/owner/repo/releases/download/v{VERSION}\n".encode(),
      )
      self.assertIn(b'version = "1.2.3"', plugin.read("datapier/plugin.toml"))
      self.assertEqual(plugin.read("datapier/LICENSE"), b"fixture license\n")
      self.assertEqual(plugin.read("datapier/content.txt"), b"fixture plugin\n")
    self.assertEqual(release["name"], "Datapier " + VERSION)
    self.assertEqual(release["tag_name"], "v" + VERSION)
    self.assertEqual(release["target_commitish"], COMMIT)


if __name__ == "__main__":
  unittest.main()

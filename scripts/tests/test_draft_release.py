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


def names(target):
  prefix = f"datapier-{VERSION}-{target}"
  ext = "zip" if target.endswith("windows-msvc") else "tar.gz"
  return f"{prefix}.{ext}", f"{prefix}.binary-sha256"


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
            release.update(json.loads(self.body()))
            return self.reply(200, release)
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
    plugin = self.root / "plugins" / "datapier"
    plugin.mkdir(parents=True)
    (plugin / "plugin.toml").write_text('name = "datapier"\nversion = "0.0.1"\n')
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
    archive, sidecar = names(target)
    payload = (f"public fixture archive for {target}\n").encode()
    binary_digest = hashlib.sha256(("binary " + target).encode()).hexdigest()
    (self.artifacts / archive).write_bytes(payload)
    (self.artifacts / sidecar).write_text(binary_digest + "\n")
    return archive, payload, binary_digest

  def upload_all(self, release):
    expected = {}
    for target in TARGETS:
      archive, payload, binary_digest = self.write_target(target)
      self.cli("upload-target", "--release-id", release["id"], "--target", target)
      expected[target] = (archive, payload, binary_digest)
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
    self.server.add_asset(release["id"], names(TARGETS[0])[0], b"old")
    self.cli("prepare", run="101")
    self.assertEqual(len(self.server.releases), 106)
    self.assertEqual(len(self.server.assets), 105)
    self.assertNotIn(names(TARGETS[0])[0], self.server.by_name())
    self.assertEqual(self.server.by_name()["manual-104.txt"], b"104")

  def test_upload_rerun_replaces_only_its_own_assets(self):
    release = self.prepare()
    archive, payload, digest = self.write_target(TARGETS[0])
    self.cli("upload-target", "--release-id", release["id"], "--target", TARGETS[0])
    self.server.add_asset(release["id"], "manual.txt", b"keep")
    (self.artifacts / archive).write_bytes(payload + b"updated")
    self.cli("upload-target", "--release-id", release["id"], "--target", TARGETS[0])
    self.assertEqual(
      self.server.by_name(),
      {
        archive: payload + b"updated",
        names(TARGETS[0])[1]: (digest + "\n").encode(),
        "manual.txt": b"keep",
      },
    )

  def test_prepare_creates_draft_and_outputs_identity(self):
    release = self.prepare()
    self.assertTrue(release["draft"])
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
      for name in names(target):
        self.server.add_asset(release["id"], name, b"old")
    for name in ("SHA256SUMS", f"datapier-plugin-{VERSION}.zip"):
      self.server.add_asset(release["id"], name, b"old")
    self.server.add_asset(release["id"], "handwritten.txt", b"keep")
    self.server.add_asset(
      release["id"], "datapier-9.9.9-x86_64-apple-darwin.tar.gz", b"other version"
    )
    self.cli("prepare", run="101", attempt="2")
    self.assertIn("Human release notes", release["body"])
    self.assertIn(f"<!-- datapier-build:{COMMIT}:101:2 -->", release["body"])
    self.assertNotIn(f"<!-- datapier-build:{COMMIT}:100:1 -->", release["body"])
    self.assertEqual(
      self.server.by_name(),
      {
        "handwritten.txt": b"keep",
        "datapier-9.9.9-x86_64-apple-darwin.tar.gz": b"other version",
      },
    )

  def test_published_release_is_not_modified(self):
    release = self.prepare()
    release["draft"] = False
    self.server.add_asset(release["id"], names(TARGETS[0])[0], b"keep")
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
    archive, sidecar = names(TARGETS[0])
    self.server.add_asset(release["id"], archive, b"archive")
    self.server.add_asset(release["id"], sidecar, b"0" * 64)
    before = copy.deepcopy((self.server.releases, self.server.assets))
    self.cli("assemble", "--release-id", release["id"], success=False)
    self.assertEqual((self.server.releases, self.server.assets), before)
    self.assertFalse((self.artifacts / "SHA256SUMS").exists())
    self.assertFalse((self.artifacts / f"datapier-plugin-{VERSION}.zip").exists())

  def test_assemble_rejects_corrupt_github_digest_without_final_assets(self):
    release = self.prepare()
    self.upload_all(release)
    next(iter(self.server.assets.values()))["digest"] = "sha256:" + "0" * 64
    before = copy.deepcopy((self.server.releases, self.server.assets))
    self.cli("assemble", "--release-id", release["id"], success=False)
    self.assertEqual((self.server.releases, self.server.assets), before)
    self.assertFalse((self.artifacts / "SHA256SUMS").exists())
    self.assertFalse((self.artifacts / f"datapier-plugin-{VERSION}.zip").exists())

  def test_complete_release_has_valid_manifest_and_plugin_metadata(self):
    release = self.prepare()
    expected = self.upload_all(release)
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
      for name, data, _ in expected.values()
    )
    binaries = "".join(
      f"{digest}  {target}\n" for target, (_, _, digest) in expected.items()
    )
    self.assertEqual(assets["SHA256SUMS"], manifest.encode())
    for target, (name, data, digest) in expected.items():
      self.assertEqual(assets[name], data)
      self.assertEqual(assets[names(target)[1]], (digest + "\n").encode())
    with zipfile.ZipFile(
      io.BytesIO(assets[f"datapier-plugin-{VERSION}.zip"])
    ) as plugin:
      self.assertEqual(plugin.read("datapier/release-sha256.txt"), manifest.encode())
      self.assertEqual(
        plugin.read("datapier/release-bin-sha256.txt"), binaries.encode()
      )
      self.assertEqual(
        plugin.read("datapier/release-url.txt"),
        f"https://github.com/owner/repo/releases/download/v{VERSION}\n".encode(),
      )
      self.assertIn(b'version = "1.2.3"', plugin.read("datapier/plugin.toml"))
      self.assertEqual(plugin.read("datapier/LICENSE"), b"fixture license\n")
      self.assertEqual(plugin.read("datapier/content.txt"), b"fixture plugin\n")


if __name__ == "__main__":
  unittest.main()

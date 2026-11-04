"""构建并汇总同一 workflow run 的 GitHub Release 草稿资源。"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import quote, urlencode, urljoin, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

from version import ROOT, check_release

TARGETS = (
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
)
BUILD_MARKER = re.compile(r"<!-- datapier-build:[^\r\n]*? -->")
STATUS_BLOCK = re.compile(
  r"<!-- datapier-status:start -->.*?<!-- datapier-status:end -->", re.DOTALL
)


def target_name(version, target):
  extension = "zip" if target.endswith("-windows-msvc") else "tar.gz"
  return f"datapier-{version}-{target}.{extension}"


def managed_body(body, marker, complete):
  # 只替换脚本拥有的标记和状态区，人工发布说明原样保留。
  body = BUILD_MARKER.sub("", body or "")
  body = STATUS_BLOCK.sub("", body)
  status = "构建完成；Release 仍为草稿。" if complete else "构建中。"
  block = (
    f"{marker}\n<!-- datapier-status:start -->\n{status}\n<!-- datapier-status:end -->"
  )
  separator = "\n" if body.endswith("\n") else "\n\n"
  return f"{body}{separator}{block}" if body else block


def origin(url):
  parts = urlsplit(url)
  return (parts.scheme.lower(), parts.hostname, parts.port)


class SafeRedirect(HTTPRedirectHandler):
  def redirect_request(self, req, fp, code, msg, headers, newurl):
    redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
    # 资源下载可能跳转至对象存储，不能把 GitHub 凭据转发到其他源。
    if redirected is not None and origin(req.full_url) != origin(newurl):
      redirected.remove_header("Authorization")
    return redirected


class GitHub:
  """执行当前发布流程所需的 GitHub REST 请求。"""

  def __init__(self, api_url, repository, token):
    self.base = api_url.rstrip("/")
    self.repo = f"/repos/{repository}"
    self.token = token
    self.opener = build_opener(SafeRedirect())

  def request(self, method, url, data=None, headers=None, missing_ok=False):
    if url.startswith("/"):
      url = self.base + url
    request_headers = {
      "Accept": "application/vnd.github+json",
      "Authorization": f"Bearer {self.token}",
      "X-GitHub-Api-Version": "2022-11-28",
      "User-Agent": "datapier-draft-release",
    }
    request_headers.update(headers or {})
    request = Request(url, data=data, headers=request_headers, method=method)
    try:
      response = self.opener.open(request)
      expected = {"GET": 200, "POST": 201, "PATCH": 200, "DELETE": 204}[method]
      if response.status != expected:
        status = response.status
        detail = response.read(65536).decode("utf-8", errors="replace")
        response.close()
        raise RuntimeError(
          f"GitHub HTTP {status}: {detail.replace(self.token, '[REDACTED]')}"
        )
      return response
    except HTTPError as error:
      if missing_ok and error.code == 404:
        error.close()
        return None
      try:
        detail = error.read(65536).decode("utf-8", errors="replace")
      finally:
        error.close()
      detail = detail.replace(self.token, "[REDACTED]")
      raise RuntimeError(f"GitHub HTTP {error.code}: {detail}") from None

  def json(self, method, path, payload=None, missing_ok=False):
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    headers = {"Content-Type": "application/json"} if data is not None else None
    response = self.request(method, path, data, headers, missing_ok)
    if response is None:
      return None
    with response:
      return json.load(response)

  def pages(self, path):
    url = self.base + path + ("&" if "?" in path else "?") + "per_page=100"
    while url:
      # API 的分页链接不应将凭据引向其他服务。
      if origin(url) != origin(self.base):
        raise ValueError("GitHub pagination points to a different origin")
      with self.request("GET", url) as response:
        items = json.load(response)
        link = response.headers.get("Link", "")
      if not isinstance(items, list):
        raise ValueError("GitHub returned an invalid list")
      yield from items
      url = None
      for part in link.split(","):
        match = re.match(r'\s*<([^>]+)>\s*;\s*rel="?next"?', part)
        if match:
          url = urljoin(self.base + "/", match.group(1))
          break

  def releases(self):
    return self.pages(f"{self.repo}/releases")

  def release(self, release_id):
    return self.json("GET", f"{self.repo}/releases/{release_id}")

  def assets(self, release_id):
    return list(self.pages(f"{self.repo}/releases/{release_id}/assets"))

  def tag_commit(self, tag):
    ref = self.json(
      "GET", f"{self.repo}/git/ref/tags/{quote(tag, safe='')}", missing_ok=True
    )
    if ref is None:
      return None
    obj = ref["object"]
    seen = set()
    # annotated tag 可嵌套，必须解析到实际 commit，不能把 tag 对象 SHA 当成提交。
    while obj["type"] == "tag":
      sha = obj["sha"]
      if sha in seen:
        raise ValueError("Git tag references form a cycle")
      seen.add(sha)
      obj = self.json("GET", f"{self.repo}/git/tags/{quote(sha, safe='')}")["object"]
    if obj["type"] != "commit":
      raise ValueError("Git tag does not point to a commit")
    return obj["sha"]

  def upload(self, release, path):
    url = release["upload_url"].split("{", 1)[0]
    url += ("&" if "?" in url else "?") + urlencode({"name": path.name})
    with path.open("rb") as source:
      with self.request(
        "POST",
        url,
        source,
        {
          "Content-Type": "application/octet-stream",
          "Content-Length": str(path.stat().st_size),
        },
      ) as response:
        asset = json.load(response)
    if asset.get("name") != path.name or asset.get("state") != "uploaded":
      raise ValueError(f"GitHub did not finish uploading {path.name}")

  def download(self, asset, path):
    with self.request(
      "GET",
      f"{self.repo}/releases/assets/{asset['id']}",
      headers={"Accept": "application/octet-stream"},
    ) as response:
      with path.open("xb") as destination:
        shutil.copyfileobj(response, destination, length=1024 * 1024)
    digest = asset.get("digest")
    if digest is not None:
      if not isinstance(digest, str) or not re.fullmatch(
        r"sha256:[0-9a-f]{64}", digest
      ):
        raise ValueError(f"Invalid GitHub digest for {asset['name']}")
      with path.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
      if actual != digest[7:]:
        raise ValueError(f"GitHub digest mismatch for {asset['name']}")


class Draft:
  """保持版本、提交及 workflow run 的草稿身份一致。"""

  def __init__(self, api, args, version):
    self.api = api
    self.args = args
    self.version = version
    self.tag = f"v{version}"
    self.name = f"Datapier {version}"
    self.marker = f"<!-- datapier-build:{args.commit}:{args.run_id}:{args.attempt} -->"
    self.target_assets = [target_name(version, target) for target in TARGETS]
    self.final_assets = [f"datapier-plugin-{version}.zip"]

  def verify(self, release_id):
    release = self.api.release(release_id)
    markers = BUILD_MARKER.findall(release.get("body") or "")
    if (
      release.get("draft") is not True
      or release.get("name") != self.name
      or release.get("tag_name") != self.tag
      or release.get("target_commitish") != self.args.commit
      or markers != [self.marker]
    ):
      raise ValueError(
        "Release draft identity, commit, or build marker does not match this run"
      )
    return release

  def remove_assets(self, release_id, names):
    for asset in self.api.assets(release_id):
      if asset["name"] in names:
        # 每次写入前重新确认草稿，避免已结束的 run 覆盖新 run 或已发布版本。
        self.verify(release_id)
        response = self.api.request(
          "DELETE", f"{self.api.repo}/releases/assets/{asset['id']}"
        )
        response.close()

  def prepare(self):
    tagged = self.api.tag_commit(self.tag)
    if tagged is not None and tagged.lower() != self.args.commit.lower():
      raise ValueError(f"Git tag {self.tag} points to a different commit")
    matches = [
      release
      for release in self.api.releases()
      if release.get("name") == self.name or release.get("tag_name") == self.tag
    ]
    if len(matches) > 1:
      ids = ", ".join(str(release["id"]) for release in matches)
      raise ValueError(f"Multiple releases found for {self.tag}: {ids}")
    if matches:
      current = self.api.release(matches[0]["id"])
      if current.get("draft") is not True:
        raise ValueError(f"Release {self.tag} is not a draft")
      current_tag = current.get("tag_name") or ""
      if current_tag != self.tag and not (
        current.get("name") == self.name and current_tag.startswith("untagged-")
      ):
        raise ValueError(f"Release {current['id']} has a conflicting tag")
      if len(BUILD_MARKER.findall(current.get("body") or "")) != 1:
        raise ValueError(f"Release {current['id']} is not managed by Datapier")
      release = self.api.json(
        "PATCH",
        f"{self.api.repo}/releases/{current['id']}",
        {
          "name": self.name,
          "tag_name": self.tag,
          "target_commitish": self.args.commit,
          "body": managed_body(current.get("body"), self.marker, False),
        },
      )
    else:
      release = self.api.json(
        "POST",
        f"{self.api.repo}/releases",
        {
          "tag_name": self.tag,
          "target_commitish": self.args.commit,
          "name": self.name,
          "draft": True,
          "body": managed_body("", self.marker, False),
        },
      )
    release_id = release["id"]
    self.verify(release_id)
    self.remove_assets(release_id, set(self.target_assets + self.final_assets))
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
      with open(output, "a", encoding="utf-8", newline="\n") as stream:
        stream.write(f"release-id={release_id}\ntag={self.tag}\n")
    else:
      print(f"release-id={release_id}\ntag={self.tag}")

  def upload_files(self, release_id, paths):
    for path in paths:
      if not path.is_file():
        raise FileNotFoundError(f"Missing release artifact: {path}")
    for path in paths:
      self.verify(release_id)
      self.remove_assets(release_id, {path.name})
      release = self.verify(release_id)
      self.api.upload(release, path)

  def upload_target(self):
    directory = Path(self.args.artifacts)
    self.upload_files(
      self.args.release_id,
      [directory / target_name(self.version, self.args.target)],
    )

  def assemble(self):
    release_id = self.args.release_id
    self.verify(release_id)
    assets = self.api.assets(release_id)
    selected = {}
    for name in self.target_assets:
      matching = [asset for asset in assets if asset.get("name") == name]
      if len(matching) != 1 or matching[0].get("state") != "uploaded":
        raise ValueError(f"Expected one uploaded release asset: {name}")
      selected[name] = matching[0]
    # 临时目录独占本次下载，不能把旧 dist 文件混入六目标汇总。
    with tempfile.TemporaryDirectory(prefix="datapier-draft-") as temporary:
      directory = Path(temporary)
      for name, asset in selected.items():
        self.api.download(asset, directory / name)
      subprocess.run(
        [
          sys.executable,
          str(ROOT / "scripts/assemble-release.py"),
          "--artifacts",
          str(directory),
          "--version",
          self.version,
          "--repository",
          self.args.repository,
        ],
        check=True,
      )
      self.upload_files(release_id, [directory / name for name in self.final_assets])
      output = Path(self.args.artifacts)
      output.mkdir(parents=True, exist_ok=True)
      for name in self.final_assets:
        shutil.copyfile(directory / name, output / name)
    release = self.verify(release_id)
    self.api.json(
      "PATCH",
      f"{self.api.repo}/releases/{release_id}",
      {
        "name": self.name,
        "tag_name": self.tag,
        "target_commitish": self.args.commit,
        "body": managed_body(release.get("body"), self.marker, True),
      },
    )
    self.verify(release_id)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("command", choices=("prepare", "upload-target", "assemble"))
  parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"))
  parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA"))
  parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID"))
  parser.add_argument("--attempt", default=os.environ.get("GITHUB_RUN_ATTEMPT"))
  parser.add_argument(
    "--api-url", default=os.environ.get("GITHUB_API_URL") or "https://api.github.com"
  )
  parser.add_argument("--artifacts", default="dist")
  parser.add_argument("--release-id")
  parser.add_argument("--target", choices=TARGETS)
  args = parser.parse_args()
  if not args.repository or not re.fullmatch(
    r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository
  ):
    parser.error("--repository or GITHUB_REPOSITORY must identify owner/repository")
  if not args.commit or not re.fullmatch(r"[0-9a-fA-F]{40}", args.commit):
    parser.error("--commit or GITHUB_SHA must be a full commit SHA")
  if not args.run_id or not re.fullmatch(r"[0-9]+", args.run_id):
    parser.error("--run-id or GITHUB_RUN_ID must be numeric")
  if not args.attempt or not re.fullmatch(r"[0-9]+", args.attempt):
    parser.error("--attempt or GITHUB_RUN_ATTEMPT must be numeric")
  if args.command != "prepare" and (
    not args.release_id or not args.release_id.isdecimal()
  ):
    parser.error("--release-id is required and must be numeric")
  if args.command == "upload-target" and not args.target:
    parser.error("--target is required for upload-target")
  version = check_release()
  token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
  if not token:
    parser.error("GH_TOKEN or GITHUB_TOKEN is required")
  draft = Draft(GitHub(args.api_url, args.repository, token), args, version)
  {
    "prepare": draft.prepare,
    "upload-target": draft.upload_target,
    "assemble": draft.assemble,
  }[args.command]()


if __name__ == "__main__":
  try:
    main()
  except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
    print(f"draft_release: {error}", file=sys.stderr)
    sys.exit(1)

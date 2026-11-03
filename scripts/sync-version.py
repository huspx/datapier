"""将插件清单版本与工作区版本同步。"""

import argparse

from version import ROOT, product_version, set_plugin_version


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument(
    "--check", action="store_true", help="check without changing the manifest"
  )
  args = parser.parse_args()
  set_plugin_version(
    ROOT / "plugins/datapier/plugin.toml", product_version(), check=args.check
  )


if __name__ == "__main__":
  main()

#!/bin/sh
# stdout 只输出供启动 plugin 解析的 JSON；下载、校验和解包失败须明确报错。
set -eu
VERSION=0.1.0
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
PLUGIN_DIR=$(dirname "$SCRIPT_DIR")
CACHE=${DATAPIER_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME:-}/.cache}/datapier}
TMP=
json_string() {
    printf '%s' "$1" | awk 'BEGIN { ORS=""; printf "\"" } { if (NR>1) printf "\\n"; for(i=1;i<=length($0);i++) { c=substr($0,i,1); if(c=="\\") printf "\\\\"; else if(c=="\"") printf "\\\""; else if(c=="\r") printf "\\r"; else if(c=="\t") printf "\\t"; else printf "%s",c } } END { printf "\"" }'
}
REPORTED=0
fail() { REPORTED=1; printf '{"ok":false,"error":'; json_string "$1"; printf '}\n'; exit 1; }
cleanup() {
    status=$?
    trap - EXIT
    [ -z "$TMP" ] || rm -rf -- "$TMP"
    if [ "$status" -ne 0 ] && [ "$REPORTED" -eq 0 ]; then
        printf '{"ok":false,"error":"安装命令失败；请查看错误输出"}\n'
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
[ -n "${DATAPIER_CACHE_DIR:-}${XDG_CACHE_HOME:-}${HOME:-}" ] || fail '无法确定缓存目录；请设置 DATAPIER_CACHE_DIR'
hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d ' ' -f 1; else fail '需要 sha256sum 或 shasum'; fi; }
mkdir -p "$CACHE" || fail '无法创建缓存目录'
CACHE=$(CDPATH= cd -- "$CACHE" && pwd -P)
case $(uname -s) in
 Darwin) OS=apple-darwin; if [ "$(sysctl -in hw.optional.arm64 2>/dev/null || printf 0)" = 1 ]; then ARCH=aarch64; else ARCH=x86_64; fi ;;
 Linux) OS=unknown-linux-gnu; case $(uname -m) in x86_64|amd64) ARCH=x86_64;; aarch64|arm64) ARCH=aarch64;; *) fail '不支持的 Linux CPU 架构';; esac ;;
 *) fail '不支持的操作系统';;
esac
TARGET=$ARCH-$OS
# 临时目录位于目标缓存所在文件系统，使校验后的硬链接发布保持原子性。
TMP=$(mktemp -d "$CACHE/.install.XXXXXXXX") || fail '无法创建临时目录'
if [ -n "${DATAPIER_BINARY:-}" ]; then
    [ -f "$DATAPIER_BINARY" ] && [ -x "$DATAPIER_BINARY" ] || fail 'DATAPIER_BINARY 不是可执行文件'
    [ "$("$DATAPIER_BINARY" --version)" = "datapier $VERSION" ] || fail '本地 Datapier 版本不匹配'
    BINARY_HASH=$(hash "$DATAPIER_BINARY")
    DEST=$CACHE/dev-$VERSION-$TARGET-$BINARY_HASH
    if [ ! -f "$DEST/datapier" ]; then
        cp "$DATAPIER_BINARY" "$TMP/datapier"
        [ "$(hash "$TMP/datapier")" = "$BINARY_HASH" ] || fail '复制时本地程序发生变化'
    fi
else
    MANIFEST=$PLUGIN_DIR/release-sha256.txt
    BINARY_MANIFEST=$PLUGIN_DIR/release-bin-sha256.txt
    [ -f "$MANIFEST" ] && [ -f "$BINARY_MANIFEST" ] || fail '此插件尚未打包发布校验清单；请设置 DATAPIER_BINARY 使用本地程序'
    FILE=datapier-$VERSION-$TARGET.tar.gz
    DIGEST=$(awk -v name="$FILE" '$2 == name {print $1}' "$MANIFEST")
    BINARY_HASH=$(awk -v name="$TARGET" '$2 == name {print $1}' "$BINARY_MANIFEST")
    for value in "$DIGEST" "$BINARY_HASH"; do
        [ "${#value}" -eq 64 ] && [ -z "$(printf '%s' "$value" | tr -d '0123456789abcdef')" ] || fail '发布校验清单缺少目标 SHA256'
    done
    DEST=$CACHE/$VERSION/$TARGET-$DIGEST
    if [ ! -f "$DEST/datapier" ]; then
        URL=${DATAPIER_RELEASE_URL:-}
        if [ -z "$URL" ] && [ -f "$PLUGIN_DIR/release-url.txt" ]; then URL=$(cat "$PLUGIN_DIR/release-url.txt"); fi
        case $URL in https://*) ;; *) fail '未配置有效的 HTTPS 发布地址；请设置 DATAPIER_RELEASE_URL';; esac
        command -v curl >/dev/null 2>&1 || fail '需要 curl 下载发布文件'
        curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 --silent --show-error "${URL%/}/$FILE" -o "$TMP/package.tar.gz" || fail '下载发布文件失败'
        # 归档摘要验证下载内容，程序摘要再次验证解包结果；固定成员名也避免路径穿越。
        [ "$(hash "$TMP/package.tar.gz")" = "$DIGEST" ] || fail '发布文件 SHA256 不匹配'
        [ "$(tar -tzf "$TMP/package.tar.gz")" = datapier ] || fail '发布压缩包内容无效'
        tar -xOzf "$TMP/package.tar.gz" datapier > "$TMP/datapier" || fail '解包失败'
        [ "$(hash "$TMP/datapier")" = "$BINARY_HASH" ] || fail '发布程序 SHA256 不匹配'
    fi
fi
if [ -f "$TMP/datapier" ]; then
    chmod 755 "$TMP/datapier"
    [ "$("$TMP/datapier" --version)" = "datapier $VERSION" ] || fail '程序版本不匹配'
    mkdir -p "$DEST"
    # 硬链接只发布完整且已校验的文件；并发安装若已有胜出者，下面仍重新校验该缓存。
    ln "$TMP/datapier" "$DEST/datapier" 2>/dev/null || [ -f "$DEST/datapier" ] || fail '无法发布缓存程序'
fi
[ -f "$DEST/datapier" ] && [ -x "$DEST/datapier" ] || fail '缓存程序不存在或不可执行'
[ "$(hash "$DEST/datapier")" = "$BINARY_HASH" ] || fail '缓存程序校验失败，请移除对应缓存目录后重试'
[ "$("$DEST/datapier" --version)" = "datapier $VERSION" ] || fail '缓存程序版本不匹配'
printf '{"ok":true,"cwd":'; json_string "$DEST"; printf '}\n'

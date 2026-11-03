# 安装结果只通过一条 JSON 交给 plugin；缓存与临时数据不写入 plugin 源码目录。
$ErrorActionPreference = 'Stop'
# 宿主按 UTF-8 解码 JSON，不能沿用 Windows PowerShell 的代码页输出。
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$lock = $null
$tmp = $null

try {
    $plugin = Split-Path $PSScriptRoot -Parent
    $pluginText = [System.IO.File]::ReadAllText((Join-Path $plugin 'plugin.toml'))
    $firstTable = [regex]::Match($pluginText, '(?m)^[ \t]*\[')
    $top = if ($firstTable.Success) { $pluginText.Substring(0, $firstTable.Index) } else { $pluginText }
    $keys = [regex]::Matches($top, '(?m)^[ \t]*version[ \t]*=')
    $values = [regex]::Matches($top, '(?m)^[ \t]*version[ \t]*=[ \t]*"((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"[ \t]*(?:#.*)?\r?$')
    if ($keys.Count -ne 1 -or $values.Count -ne 1) { throw '插件版本缺失、重复或无效' }
    $version = $values[0].Groups[1].Value
    # GetNativeSystemInfo 取得硬件架构，避免在 ARM Windows 的兼容进程中误选 x86 包。
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class DatapierNative
{
    [StructLayout(LayoutKind.Sequential)]
    public struct SYSTEM_INFO
    {
        public ushort architecture, reserved;
        public uint pageSize;
        public IntPtr minimum, maximum, mask;
        public uint processors, type, granularity;
        public ushort level, revision;
    }

    [DllImport("kernel32.dll")]
    public static extern void GetNativeSystemInfo(out SYSTEM_INFO info);
}
'@
    $info = New-Object DatapierNative+SYSTEM_INFO
    [DatapierNative]::GetNativeSystemInfo([ref]$info)
    # Win32 PROCESSOR_ARCHITECTURE_AMD64=9、ARM64=12，不根据当前 PowerShell 位数猜测。
    $arch = switch ($info.architecture) {
        9 {
            'x86_64'
        }
        12 {
            'aarch64'
        }
        default {
            throw '不支持的 Windows CPU 架构'
        }
    }
    $target = "$arch-pc-windows-msvc"
    $cache = if ($env:DATAPIER_CACHE_DIR) {
        $env:DATAPIER_CACHE_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'Datapier\cache'
    }
    [System.IO.Directory]::CreateDirectory($cache) | Out-Null
    $cache = [System.IO.Path]::GetFullPath($cache)
    # FileShare.None 让锁随进程句柄释放，避免目录锁在崩溃后永久阻塞后续安装。
    for ($attempt = 0; $attempt -lt 120; $attempt++) {
        try {
            $lock = [System.IO.File]::Open((Join-Path $cache '.install.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
            break
        } catch [System.IO.IOException] {
            Start-Sleep -Seconds 1
        }
    }
    if (!$lock) {
        throw '等待安装锁超时'
    }
    $tmp = Join-Path $cache ('.install-' + [guid]::NewGuid().ToString('N'))
    [System.IO.Directory]::CreateDirectory($tmp) | Out-Null
    if ($env:DATAPIER_BINARY) {
        $binary = [System.IO.Path]::GetFullPath($env:DATAPIER_BINARY)
        if (!(Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw 'DATAPIER_BINARY 不是文件'
        }
        $output = & $binary --version
        if ($LASTEXITCODE -ne 0 -or "$output".Trim() -ne "datapier $version") {
            throw '本地 Datapier 版本不匹配'
        }
        $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $binary).Hash.ToLowerInvariant()
        $binaryHash = $digest
        # 按内容寻址写入新目录，避免替换仍在运行、被 Windows 锁定的 exe。
        $dest = Join-Path $cache "dev-$version-$target-$digest"
        if (!(Test-Path -LiteralPath (Join-Path $dest 'datapier.exe'))) {
            Copy-Item -LiteralPath $binary -Destination (Join-Path $tmp 'datapier.exe')
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp 'datapier.exe')).Hash.ToLowerInvariant() -ne $binaryHash) {
                throw '复制时本地程序发生变化'
            }
            [System.IO.Directory]::Move($tmp, $dest)
            $tmp = $null
        }
    } else {
        $manifest = Join-Path $plugin 'release-sha256.txt'
        $binaryManifest = Join-Path $plugin 'release-bin-sha256.txt'
        if (!(Test-Path -LiteralPath $manifest -PathType Leaf) -or !(Test-Path -LiteralPath $binaryManifest -PathType Leaf)) {
            throw '此插件尚未打包发布校验清单；请设置 DATAPIER_BINARY 使用本地程序'
        }
        $file = "datapier-$version-$target.zip"
        $matches = @(Get-Content -LiteralPath $manifest | Where-Object {
            $_ -match ('^([0-9a-f]{64})  ' + [regex]::Escape($file) + '$')
        })
        if ($matches.Count -ne 1) {
            throw '发布校验清单缺少唯一的目标 SHA256'
        }
        $digest = $matches[0].Substring(0, 64)
        $binaryLines = @(Get-Content -LiteralPath $binaryManifest | Where-Object {
            $_ -match ('^([0-9a-f]{64})  ' + [regex]::Escape($target) + '$')
        })
        if ($binaryLines.Count -ne 1) {
            throw '发布校验清单缺少唯一的程序 SHA256'
        }
        $binaryHash = $binaryLines[0].Substring(0, 64)
        $parent = Join-Path $cache $version
        $dest = Join-Path $parent "$target-$digest"
        if (!(Test-Path -LiteralPath (Join-Path $dest 'datapier.exe'))) {
            $archive = Join-Path $tmp 'package.zip'
            $baseUrl = $env:DATAPIER_RELEASE_URL
            $urlFile = Join-Path $plugin 'release-url.txt'
            if (!$baseUrl -and (Test-Path -LiteralPath $urlFile -PathType Leaf)) {
                $baseUrl = ([System.IO.File]::ReadAllText($urlFile)).Trim()
            }
            if (!$baseUrl -or !$baseUrl.StartsWith('https://')) {
                throw '未配置有效的 HTTPS 发布地址；请设置 DATAPIER_RELEASE_URL'
            }
            $url = $baseUrl.TrimEnd('/') + '/' + $file
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archive
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant() -ne $digest) {
                throw '发布文件 SHA256 不匹配'
            }
            # 不信任 ZIP 中的相对路径；仅接受唯一的固定文件，再核对程序自身摘要。
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
            try {
                if ($zip.Entries.Count -ne 1 -or $zip.Entries[0].FullName -cne 'datapier.exe') {
                    throw '发布压缩包内容无效'
                }
                [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
                    $zip.Entries[0],
                    (Join-Path $tmp 'datapier.exe')
                )
            } finally {
                $zip.Dispose()
            }
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp 'datapier.exe')).Hash.ToLowerInvariant() -ne $binaryHash) {
                throw '发布程序 SHA256 不匹配'
            }
            $output = & (Join-Path $tmp 'datapier.exe') --version
            if ($LASTEXITCODE -ne 0 -or "$output".Trim() -ne "datapier $version") {
                throw '下载程序版本不匹配'
            }
            Remove-Item -LiteralPath $archive
            [System.IO.Directory]::CreateDirectory($parent) | Out-Null
            [System.IO.Directory]::Move($tmp, $dest)
            $tmp = $null
        }
    }
    $cached = Join-Path $dest 'datapier.exe'
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $cached).Hash.ToLowerInvariant() -ne $binaryHash) {
        throw '缓存程序校验失败，请移除对应缓存目录后重试'
    }
    $output = & $cached --version
    if ($LASTEXITCODE -ne 0 -or "$output".Trim() -ne "datapier $version") {
        throw '缓存程序版本不匹配'
    }
    @{ ok = $true; cwd = $dest } | ConvertTo-Json -Compress
    exit 0
} catch {
    @{ ok = $false; error = $_.Exception.Message } | ConvertTo-Json -Compress
    exit 1
} finally {
    if ($tmp -and (Test-Path -LiteralPath $tmp)) {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($lock) {
        $lock.Dispose()
    }
}

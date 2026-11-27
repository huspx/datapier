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
    # 模拟进程也使用系统返回的 nativeMachine 选择目标架构。
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class DatapierNative
{
    [DllImport("kernel32.dll")]
    public static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool IsWow64Process2(IntPtr process, out ushort processMachine, out ushort nativeMachine);
}
'@
    [uint16]$processMachine = 0
    [uint16]$nativeMachine = 0
    if (![DatapierNative]::IsWow64Process2([DatapierNative]::GetCurrentProcess(), [ref]$processMachine, [ref]$nativeMachine)) {
        throw [System.ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
    }
    # IMAGE_FILE_MACHINE_AMD64=0x8664、ARM64=0xaa64。
    $arch = switch ($nativeMachine) {
        0x8664 {
            'x86_64'
        }
        0xaa64 {
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
        $launcherManifest = Join-Path $plugin 'release-launcher-sha256.txt'
        if (!(Test-Path -LiteralPath $manifest -PathType Leaf) -or !(Test-Path -LiteralPath $binaryManifest -PathType Leaf) -or !(Test-Path -LiteralPath $launcherManifest -PathType Leaf)) {
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
        $launcherLines = @(Get-Content -LiteralPath $launcherManifest | Where-Object {
            $_ -match ('^([0-9a-f]{64})  ' + [regex]::Escape($target) + '$')
        })
        if ($launcherLines.Count -ne 1) {
            throw '发布校验清单缺少唯一的 launcher SHA256'
        }
        $launcherHash = $launcherLines[0].Substring(0, 64)
        $parent = Join-Path $cache $version
        # 新清单使用独立目录，旧版只含 datapier.exe 的缓存不可冒充完整缓存。
        $dest = Join-Path $parent "$target-$digest-$launcherHash"
        if (!(Test-Path -LiteralPath (Join-Path $dest 'datapier.exe'))) {
            $archive = Join-Path $tmp 'package.zip'
            if ($env:DATAPIER_UPDATE_ARCHIVE) {
                $archive = $env:DATAPIER_UPDATE_ARCHIVE
            } else {
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
            }
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant() -ne $digest) {
                throw '发布文件 SHA256 不匹配'
            }
            # 只接受两个固定名称且唯一的普通文件，不使用归档提供的路径。
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
            try {
                $names = @('datapier.exe', 'datapier-launcher.exe')
                if ($zip.Entries.Count -ne 2) { throw '发布压缩包内容无效' }
                foreach ($name in $names) {
                    $entries = @($zip.Entries | Where-Object { $_.FullName -ceq $name })
                    if ($entries.Count -ne 1) { throw '发布压缩包内容无效' }
                    $mode = ($entries[0].ExternalAttributes -shr 16) -band 0xF000
                    if ($mode -ne 0 -and $mode -ne 0x8000) { throw '发布压缩包成员不是普通文件' }
                    [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
                        $entries[0], (Join-Path $tmp $name)
                    )
                }
            } finally {
                $zip.Dispose()
            }
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp 'datapier.exe')).Hash.ToLowerInvariant() -ne $binaryHash) {
                throw '发布程序 SHA256 不匹配'
            }
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp 'datapier-launcher.exe')).Hash.ToLowerInvariant() -ne $launcherHash) {
                throw '发布 launcher SHA256 不匹配'
            }
            $output = & (Join-Path $tmp 'datapier.exe') --version
            if ($LASTEXITCODE -ne 0 -or "$output".Trim() -ne "datapier $version") {
                throw '下载程序版本不匹配'
            }
            if (!$env:DATAPIER_UPDATE_ARCHIVE) { Remove-Item -LiteralPath $archive }
            [System.IO.Directory]::CreateDirectory($parent) | Out-Null
            [System.IO.Directory]::Move($tmp, $dest)
            $tmp = $null
        }
    }
    if (!$env:DATAPIER_BINARY) {
        $cachedLauncher = Join-Path $dest 'datapier-launcher.exe'
        if (!(Test-Path -LiteralPath $cachedLauncher -PathType Leaf) -or (Get-FileHash -Algorithm SHA256 -LiteralPath $cachedLauncher).Hash.ToLowerInvariant() -ne $launcherHash) {
            throw '缓存 launcher 校验失败，请移除对应缓存目录后重试'
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

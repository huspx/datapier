# 只组装对应原生 runner 已构建的程序；组包成功本身不构成该平台运行验收。
param(
    [Parameter(Mandatory=$true)][ValidateSet('x86_64-apple-darwin','aarch64-apple-darwin','x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu','x86_64-pc-windows-msvc','aarch64-pc-windows-msvc')][string]$Target,
    [string]$Version = '0.1.0',
    [string]$Output = 'dist'
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid release version' }
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$out = if ([System.IO.Path]::IsPathRooted($Output)) { [System.IO.Path]::GetFullPath($Output) } else { [System.IO.Path]::GetFullPath((Join-Path $root $Output)) }
[System.IO.Directory]::CreateDirectory($out) | Out-Null
$windows = $Target.EndsWith('-windows-msvc')
$name = if ($windows) { 'datapier.exe' } else { 'datapier' }
$binary = Join-Path $root "target/$Target/release/$name"
if (!(Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing built binary: $binary" }
$temp = Join-Path ([System.IO.Path]::GetTempPath()) ('datapier-package-' + [guid]::NewGuid().ToString('N'))
[System.IO.Directory]::CreateDirectory($temp) | Out-Null
try {
    Copy-Item -LiteralPath $binary -Destination (Join-Path $temp $name)
    # sidecar 描述实际入包程序，与稍后汇总生成的归档摘要分别验证，不能相互替代。
    $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $temp $name)).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText((Join-Path $out "datapier-$Version-$Target.binary-sha256"), $digest + "`n", [System.Text.UTF8Encoding]::new($false))
    if ($windows) {
        Compress-Archive -LiteralPath (Join-Path $temp $name) -DestinationPath (Join-Path $out "datapier-$Version-$Target.zip")
    } else {
        $previousCopyfileDisable = $env:COPYFILE_DISABLE
        try {
            # macOS 的 AppleDouble 元数据会破坏安装器要求的单成员归档约定。
            $env:COPYFILE_DISABLE = '1'
            & tar -czf (Join-Path $out "datapier-$Version-$Target.tar.gz") -C $temp datapier
            if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
        } finally { $env:COPYFILE_DISABLE = $previousCopyfileDisable }
    }
} finally { Remove-Item -LiteralPath $temp -Recurse -Force }

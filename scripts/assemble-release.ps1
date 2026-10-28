# 六个原生产物及程序摘要齐全后才生成分发清单；本脚本不创建 Release 或上传资源。
param([string]$Artifacts = 'dist', [string]$Version = '0.1.0', [string]$Repository = $env:GITHUB_REPOSITORY)
$ErrorActionPreference = 'Stop'
# plugin 和安装器也内嵌版本，不能仅覆盖命令行参数而混用另一版本的启动入口。
if ($Version -ne '0.1.0') { throw 'Update plugin and installer version before packaging another release' }
if (!$Repository -or $Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw 'Set -Repository or GITHUB_REPOSITORY to the actual GitHub owner/repository' }
$releaseUrl = "https://github.com/$Repository/releases/download/v$Version"
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dir = (Resolve-Path $Artifacts).Path
$targets = @('x86_64-apple-darwin','aarch64-apple-darwin','x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu','x86_64-pc-windows-msvc','aarch64-pc-windows-msvc')
# 先完成全部输入校验，再写汇总产物，避免缺目标时产生看似完整的启动包。
$lines = foreach ($target in $targets) {
    $ext = if ($target.EndsWith('-windows-msvc')) { 'zip' } else { 'tar.gz' }
    $name = "datapier-$Version-$target.$ext"
    $path = Join-Path $dir $name
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing release artifact: $name" }
    ((Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant() + '  ' + $name)
}
$binaryLines = foreach ($target in $targets) {
    $sidecar = Join-Path $dir "datapier-$Version-$target.binary-sha256"
    if (!(Test-Path -LiteralPath $sidecar -PathType Leaf)) { throw "Missing binary digest: $target" }
    $digest = ([System.IO.File]::ReadAllText($sidecar)).Trim()
    if ($digest -cnotmatch '^[0-9a-f]{64}$') { throw "Invalid binary digest: $target" }
    "$digest  $target"
}
$manifest = ($lines -join "`n") + "`n"
[System.IO.File]::WriteAllText((Join-Path $dir 'SHA256SUMS'), $manifest, [System.Text.UTF8Encoding]::new($false))
$temp = Join-Path ([System.IO.Path]::GetTempPath()) ('datapier-release-' + [guid]::NewGuid().ToString('N'))
try {
    [System.IO.Directory]::CreateDirectory($temp) | Out-Null
    Copy-Item -LiteralPath (Join-Path $root 'plugins/datapier') -Destination (Join-Path $temp 'datapier') -Recurse
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $temp 'datapier/LICENSE')
    [System.IO.File]::WriteAllText((Join-Path $temp 'datapier/release-sha256.txt'), $manifest, [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText((Join-Path $temp 'datapier/release-bin-sha256.txt'), (($binaryLines -join "`n") + "`n"), [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText((Join-Path $temp 'datapier/release-url.txt'), ($releaseUrl + "`n"), [System.Text.UTF8Encoding]::new($false))
    Compress-Archive -LiteralPath (Join-Path $temp 'datapier') -DestinationPath (Join-Path $dir "datapier-plugin-$Version.zip")
} finally { if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force } }

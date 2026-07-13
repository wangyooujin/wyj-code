# wyj-code 一键安装引导脚本（Windows）—— irm <url>/install.ps1 | iex
#
# 与仓库根目录的 install.bat 不是同一个文件：那个脚本随 release 压缩包分发，假定
# wyj-code.exe 已经和它解压在同一目录，只负责"装到 %USERPROFILE%\.wyj-code\bin +
# 配置用户级 PATH"。这个脚本运行在用户还什么都没有的机器上，只做前置工作——从
# GitHub Releases 重定向解析最新版本、下载对应压缩包、校验 sha256、解压——解压完成后
# 直接调用包内那个 install.bat，把安装这一步完全交给它，不重复实现一遍。

$ErrorActionPreference = "Stop"

$RepoOwner = "wangyooujin"
$RepoName = "wyj-code"
$Binary = "wyj-code"
# self_update.rs::target_for 里 Windows 只映射了这一个 target
$Target = "x86_64-pc-windows-msvc"

function Fail($msg) {
    Write-Host "错误：$msg" -ForegroundColor Red
    Write-Host "可以去 https://github.com/$RepoOwner/$RepoName/releases 手动下载安装。"
    exit 1
}

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64" -and $env:PROCESSOR_ARCHITECTURE -ne "x86") {
    Fail "不支持的 Windows 架构：$($env:PROCESSOR_ARCHITECTURE)"
}

# 1. 版本：默认查最新 release，可用 WYJ_CODE_VERSION 环境变量指定具体版本回滚/固定
if ($env:WYJ_CODE_VERSION) {
    $Version = $env:WYJ_CODE_VERSION
    $Tag = "v$Version"
    Write-Host "==> 使用指定版本：$Version（来自 WYJ_CODE_VERSION）"
} else {
    Write-Host "==> 查询最新版本..."
    $latestUrl = "https://github.com/$RepoOwner/$RepoName/releases/latest"
    try {
        $latestResponse = Invoke-WebRequest -Uri $latestUrl -MaximumRedirection 10 -UseBasicParsing
        $latestLocation = $latestResponse.BaseResponse.ResponseUri.AbsoluteUri
    } catch {
        Fail "查询最新 release 失败：$latestUrl（$($_.Exception.Message)）"
    }
    $tagMatch = [regex]::Match($latestLocation, '/releases/tag/([^/?#]+)')
    if (-not $tagMatch.Success) { Fail "无法从 GitHub Releases 重定向地址解析版本：$latestLocation" }
    $Tag = $tagMatch.Groups[1].Value
    if ($Tag -notmatch '^v[0-9]') { Fail "GitHub Releases 返回了无效版本 tag：$Tag" }
    $Version = $Tag -replace '^v', ''
    Write-Host "==> 最新版本：$Version"
}

# 2. 拼资产名（命名规则与 .github/workflows/release.yml / asset_names() 一致）
$Archive = "$Binary-$Version-$Target.zip"
$ArchiveUrl = "https://github.com/$RepoOwner/$RepoName/releases/download/$Tag/$Archive"
$Sha256Url = "$ArchiveUrl.sha256"
$SumsUrl = "https://github.com/$RepoOwner/$RepoName/releases/download/$Tag/SHA256SUMS"

# 3. 下载 + 校验 + 解压
$TmpDir = Join-Path $env:TEMP ("wyj-code-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TmpDir | Out-Null

try {
    $ArchivePath = Join-Path $TmpDir $Archive
    $Sha256Path = "$ArchivePath.sha256"

    Write-Host "==> 下载 $ArchiveUrl"
    try {
        Invoke-WebRequest -Uri $ArchiveUrl -OutFile $ArchivePath
    } catch {
        Fail "下载安装包失败：$($_.Exception.Message)"
    }
    try {
        Invoke-WebRequest -Uri $Sha256Url -OutFile $Sha256Path
    } catch {
        Write-Host "==> 单独校验和不存在，尝试 SHA256SUMS"
        $SumsPath = Join-Path $TmpDir "SHA256SUMS"
        try {
            Invoke-WebRequest -Uri $SumsUrl -OutFile $SumsPath
        } catch {
            Fail "下载校验和文件失败：$($_.Exception.Message)"
        }
        $line = Get-Content $SumsPath | Where-Object {
            $parts = $_.Trim() -split '\s+'
            $parts.Count -ge 2 -and $parts[1].TrimStart('*') -eq $Archive
        } | Select-Object -First 1
        if (-not $line) {
            Fail "SHA256SUMS 中未找到 $Archive"
        }
        Set-Content -Path $Sha256Path -Encoding ascii -Value $line
    }

    Write-Host "==> 校验 sha256"
    $expected = (Get-Content $Sha256Path -Raw).Trim().Split(" ")[0].Trim().ToLower()
    $actual = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        Fail "sha256 校验失败：期望 $expected，实际 $actual（安装包可能已损坏或被篡改）"
    }

    Write-Host "==> 解压"
    Expand-Archive -Path $ArchivePath -DestinationPath $TmpDir -Force

    $ExtractedDir = Join-Path $TmpDir "$Binary-$Version-$Target"
    $InstallBat = Join-Path $ExtractedDir "install.bat"
    if (-not (Test-Path $InstallBat)) {
        Fail "压缩包内未找到 install.bat（归档结构异常）"
    }

    # 4. 交给压缩包里自带的 install.bat 完成实际安装（装入用户目录 + 配置 PATH）
    Write-Host "==> 执行安装..."
    Push-Location $ExtractedDir
    try {
        & cmd /c install.bat
        if ($LASTEXITCODE -ne 0) {
            Fail "install.bat 执行失败，退出码 $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}

@echo off
setlocal enabledelayedexpansion
REM wyj-code 一键安装脚本（Windows）
REM 用法：在解压出的目录下双击运行，或在终端执行 install.bat

set "BINARY=wyj-code.exe"
set "SRC=%~dp0%BINARY%"
set "DEST_DIR=%USERPROFILE%\.wyj-code\bin"
set "DEST=%DEST_DIR%\%BINARY%"

if not exist "%SRC%" (
    echo 未找到 %SRC%，请确认 install.bat 与 %BINARY% 在同一目录下（解压是否完整？）
    exit /b 1
)

if not exist "%DEST_DIR%" mkdir "%DEST_DIR%"

copy /Y "%SRC%" "%DEST%" >nul
if errorlevel 1 (
    echo 安装失败：如果 wyj-code 正在运行，请先关闭它再重试
    exit /b 1
)

echo 已安装: %DEST%
"%DEST%" --version

REM 通过 PowerShell 安全地追加到当前用户 PATH：
REM setx 会把当前进程里系统+用户合并后的 PATH 整体回写成"用户级" PATH（越写越长），
REM 且目标值超过 1024 字符会被静默截断，可能连别的软件的 PATH 项一起截没。
REM 改用 .NET Environment 只读写用户级 PATH，且提前判重，做到幂等。
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dest = '%DEST_DIR%'; $cur = [Environment]::GetEnvironmentVariable('Path','User'); if ([string]::IsNullOrEmpty($cur)) { $cur = '' }; if (($cur -split ';') -notcontains $dest) { [Environment]::SetEnvironmentVariable('Path', ($cur.TrimEnd(';') + ';' + $dest), 'User'); Write-Host ('已将 ' + $dest + ' 加入用户 PATH，重新打开终端后生效') } else { Write-Host 'PATH 已包含安装目录，跳过' }"

echo.
echo 下一步：
echo   setx WYJ_CODE_API_KEY sk-...          （或写入 %USERPROFILE%\.wyj-code\config.toml）
echo   重新打开一个终端窗口，运行 wyj-code

endlocal

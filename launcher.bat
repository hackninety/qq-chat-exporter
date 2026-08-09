@echo off
chcp 65001 >nul
setlocal EnableExtensions
cd /d "%~dp0"

title QQ Chat Exporter Launcher

if not defined QCE_PACKAGE_DIR set "QCE_PACKAGE_DIR=%~dp0NapCat-QCE-Windows-x64"
set "QCE_PACKAGE_LAUNCHER=%QCE_PACKAGE_DIR%\launcher.bat"
set "QCE_PACKAGE_BUILD_STAMP=%QCE_PACKAGE_DIR%\.qce-build-stamp"
set "QCE_QUICK_LOGIN_FILE=%USERPROFILE%\.qq-chat-exporter\qq-login.json"
set "QCE_QUICK_LOGIN_DISABLED_FILE=%USERPROFILE%\.qq-chat-exporter\qq-login.disabled"

call :restore_quick_login_account

if "%QCE_REBUILD%"=="1" goto :build_package
if exist "%QCE_PACKAGE_LAUNCHER%" call :detect_stale_package
if "%QCE_SERVER_SOURCE_STALE%"=="1" set "QCE_REBUILD_SERVER=1"
if "%QCE_PACKAGE_STALE%"=="1" (
    echo [QCE] Source changes detected. Rebuilding the Windows runtime package...
    goto :build_package
)
if exist "%QCE_PACKAGE_LAUNCHER%" (
    if "%QCE_BUILD_ONLY%"=="1" (
        echo [QCE] Windows runtime package is ready.
        exit /b 0
    )
    goto :launch_package
)

echo.
echo [QCE] The Windows runtime package has not been built yet.
echo [QCE] Building it now. The first run may download NapCat and compile QCE.
echo.

:build_package
if not exist "%~dp0scripts\quick-pack.py" (
    echo [Error] scripts\quick-pack.py was not found.
    goto :failed
)

if not defined QCE_SERVER_WINDOWS_X64 if not defined QCE_SERVER_BINARY if not "%QCE_REBUILD_SERVER%"=="1" (
    if exist "%~dp0qq-chat-export-server\target\release\qce-server.exe" (
        set "QCE_SERVER_WINDOWS_X64=%~dp0qq-chat-export-server\target\release\qce-server.exe"
    )
)

if defined QCE_SERVER_WINDOWS_X64 (
    echo [QCE] Using server binary: "%QCE_SERVER_WINDOWS_X64%"
) else if defined QCE_SERVER_BINARY (
    echo [QCE] Using server binary: "%QCE_SERVER_BINARY%"
) else (
    call :prepare_cargo
    if errorlevel 1 goto :failed
)

set "QCE_PYTHON_KIND="
set "QCE_PYTHON_EXE="

if defined QCE_PYTHON (
    if not exist "%QCE_PYTHON%" (
        echo [Error] QCE_PYTHON does not point to a file: "%QCE_PYTHON%"
        goto :failed
    )
    "%QCE_PYTHON%" -c "import sys; raise SystemExit(0 if sys.version_info.major == 3 and sys.version_info.minor in range(10, 100) else 1)" >nul 2>&1
    if errorlevel 1 (
        echo [Error] QCE_PYTHON is not a working Python 3.10+ interpreter.
        goto :failed
    )
    set "QCE_PYTHON_KIND=exe"
    set "QCE_PYTHON_EXE=%QCE_PYTHON%"
    goto :python_found
)

where py >nul 2>&1
if not errorlevel 1 (
    py -3 -c "import sys; raise SystemExit(0 if sys.version_info.major == 3 and sys.version_info.minor in range(10, 100) else 1)" >nul 2>&1
    if not errorlevel 1 (
        set "QCE_PYTHON_KIND=py"
        goto :python_found
    )
)

where python >nul 2>&1
if not errorlevel 1 (
    python -c "import sys; raise SystemExit(0 if sys.version_info.major == 3 and sys.version_info.minor in range(10, 100) else 1)" >nul 2>&1
    if not errorlevel 1 (
        set "QCE_PYTHON_KIND=exe"
        set "QCE_PYTHON_EXE=python"
        goto :python_found
    )
)

set "QCE_CODEX_PYTHON=%USERPROFILE%\.cache\codex-runtimes\codex-primary-runtime\dependencies\python\python.exe"
if exist "%QCE_CODEX_PYTHON%" (
    "%QCE_CODEX_PYTHON%" -c "import sys; raise SystemExit(0 if sys.version_info.major == 3 and sys.version_info.minor in range(10, 100) else 1)" >nul 2>&1
    if not errorlevel 1 (
        set "QCE_PYTHON_KIND=exe"
        set "QCE_PYTHON_EXE=%QCE_CODEX_PYTHON%"
        goto :python_found
    )
)

echo [Error] A working Python 3.10+ interpreter was not found.
echo         Install Python 3, or set QCE_PYTHON to python.exe and run launcher.bat again.
goto :failed

:python_found
if /i "%QCE_PYTHON_KIND%"=="py" goto :run_build_with_py

echo [QCE] Building with "%QCE_PYTHON_EXE%"...
"%QCE_PYTHON_EXE%" "%~dp0scripts\quick-pack.py"
goto :build_finished

:run_build_with_py
echo [QCE] Building with Python Launcher...
py -3 "%~dp0scripts\quick-pack.py"

:build_finished
set "QCE_BUILD_EXIT=%errorlevel%"
if not "%QCE_BUILD_EXIT%"=="0" (
    echo [Error] Failed to build the Windows runtime package ^(exit code %QCE_BUILD_EXIT%^).
    goto :failed
)

if not exist "%QCE_PACKAGE_LAUNCHER%" (
    echo [Error] Build completed without creating:
    echo         "%QCE_PACKAGE_LAUNCHER%"
    goto :failed
)

powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; [IO.File]::WriteAllText($env:QCE_PACKAGE_BUILD_STAMP, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))"
if errorlevel 1 (
    echo [Error] Failed to record the Windows runtime package build timestamp.
    goto :failed
)

if "%QCE_BUILD_ONLY%"=="1" (
    echo [QCE] Windows runtime package is ready.
    exit /b 0
)

:launch_package
if not defined NAPCAT_QUICK_ACCOUNT goto :launch_without_quick_login
echo [QCE] Reusing the local QQ login for account %NAPCAT_QUICK_ACCOUNT%.
if not "%~1"=="" goto :launch_with_quick_login
if not exist "%QCE_PACKAGE_DIR%\config\qq_path.txt" goto :launch_with_quick_login
set /p "QCE_SAVED_QQ_PATH="<"%QCE_PACKAGE_DIR%\config\qq_path.txt"
if not exist "%QCE_SAVED_QQ_PATH%" goto :launch_with_quick_login
call "%QCE_PACKAGE_LAUNCHER%" "%QCE_SAVED_QQ_PATH%" -q "%NAPCAT_QUICK_ACCOUNT%"
exit /b %errorlevel%

:launch_with_quick_login
call "%QCE_PACKAGE_LAUNCHER%" %* -q "%NAPCAT_QUICK_ACCOUNT%"
exit /b %errorlevel%

:launch_without_quick_login
echo [QCE] Starting "%QCE_PACKAGE_LAUNCHER%"...
call "%QCE_PACKAGE_LAUNCHER%" %*
exit /b %errorlevel%

:failed
echo.
pause
exit /b 1

:restore_quick_login_account
if defined NAPCAT_QUICK_ACCOUNT exit /b 0
if exist "%QCE_QUICK_LOGIN_FILE%" (
    for /f "usebackq delims=" %%i in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='SilentlyContinue'; $uin=(Get-Content -Raw -LiteralPath $env:QCE_QUICK_LOGIN_FILE | ConvertFrom-Json).uin; if ($uin -match '^\d{5,12}$') { $uin }"`) do set "NAPCAT_QUICK_ACCOUNT=%%i"
)
if defined NAPCAT_QUICK_ACCOUNT exit /b 0
if exist "%QCE_QUICK_LOGIN_DISABLED_FILE%" exit /b 0

rem One-time migration for packages created before qq-login.json existed.
if exist "%QCE_PACKAGE_DIR%\config" (
    set "QCE_PACKAGE_CONFIG=%QCE_PACKAGE_DIR%\config"
    for /f "usebackq delims=" %%i in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='SilentlyContinue'; Get-ChildItem -LiteralPath $env:QCE_PACKAGE_CONFIG -Filter 'napcat_*.json' -File | Sort-Object LastWriteTimeUtc -Descending | ForEach-Object { if ($_.BaseName -match '^napcat_(\d{5,12})$') { $Matches[1]; break } }"`) do set "NAPCAT_QUICK_ACCOUNT=%%i"
)
if defined NAPCAT_QUICK_ACCOUNT powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='SilentlyContinue'; $parent=Split-Path -Parent $env:QCE_QUICK_LOGIN_FILE; New-Item -ItemType Directory -Force -Path $parent | Out-Null; [IO.File]::WriteAllText($env:QCE_QUICK_LOGIN_FILE, (@{schemaVersion=1;uin=$env:NAPCAT_QUICK_ACCOUNT;updatedAt=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json), [Text.UTF8Encoding]::new($false))"
exit /b 0

:prepare_cargo
set "QCE_CARGO_EXE="
if not defined QCE_CARGO goto :detect_cargo
if not exist "%QCE_CARGO%" goto :invalid_cargo_override
set "QCE_CARGO_EXE=%QCE_CARGO%"
goto :cargo_found

:invalid_cargo_override
echo [Error] QCE_CARGO does not point to a file: "%QCE_CARGO%"
exit /b 1

:detect_cargo
for /f "delims=" %%i in ('where cargo 2^>nul') do if not defined QCE_CARGO_EXE set "QCE_CARGO_EXE=%%i"
if defined QCE_CARGO_EXE goto :cargo_found
if defined CARGO_HOME if exist "%CARGO_HOME%\bin\cargo.exe" set "QCE_CARGO_EXE=%CARGO_HOME%\bin\cargo.exe"
if defined QCE_CARGO_EXE goto :cargo_found
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" set "QCE_CARGO_EXE=%USERPROFILE%\.cargo\bin\cargo.exe"
if defined QCE_CARGO_EXE goto :cargo_found

echo [Error] Rust/Cargo was not found and no compiled QCE server is available.
echo         Checked PATH, CARGO_HOME and "%USERPROFILE%\.cargo\bin\cargo.exe".
echo         Install Rust from https://rustup.rs/ or set QCE_CARGO to cargo.exe.
exit /b 1

:cargo_found
"%QCE_CARGO_EXE%" --version >nul 2>&1
if errorlevel 1 goto :cargo_not_working
for %%i in ("%QCE_CARGO_EXE%") do set "PATH=%%~dpi;%PATH%"
echo [QCE] Using Rust/Cargo: "%QCE_CARGO_EXE%"
exit /b 0

:cargo_not_working
echo [Error] Rust/Cargo is not working: "%QCE_CARGO_EXE%"
exit /b 1

:detect_stale_package
set "QCE_PACKAGE_STALE=0"
set "QCE_SERVER_SOURCE_STALE=0"
if "%QCE_SKIP_AUTO_REBUILD%"=="1" exit /b 0
set "QCE_REPO_ROOT=%~dp0"
for /f "usebackq tokens=1,2 delims==" %%a in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $root=$env:QCE_REPO_ROOT; function Newer([string]$output,[string[]]$inputs) { if (-not (Test-Path -LiteralPath $output)) { return $true }; $stamp=(Get-Item -LiteralPath $output).LastWriteTimeUtc.AddSeconds(2); foreach ($input in $inputs) { if (-not (Test-Path -LiteralPath $input)) { continue }; $item=Get-Item -LiteralPath $input; if (-not $item.PSIsContainer) { if ($item.LastWriteTimeUtc -gt $stamp) { return $true }; continue }; if (Get-ChildItem -LiteralPath $input -Recurse -File | Where-Object LastWriteTimeUtc -gt $stamp | Select-Object -First 1) { return $true } }; return $false }; $frontend=@('qce-v4-tool\app','qce-v4-tool\components','qce-v4-tool\hooks','qce-v4-tool\lib','qce-v4-tool\public','qce-v4-tool\types','qce-v4-tool\package.json','qce-v4-tool\pnpm-lock.yaml','qce-v4-tool\next.config.mjs') | ForEach-Object { Join-Path $root $_ }; $plugin=@('plugins\qq-chat-exporter\index.mjs','plugins\qq-chat-exporter\runtime','plugins\qq-chat-exporter\package.json','scripts\quick-pack.py','scripts\plugin_runtime.py') | ForEach-Object { Join-Path $root $_ }; $server=@('qq-chat-export-server\src','qq-chat-export-server\Cargo.toml','qq-chat-export-server\Cargo.lock','qq-chat-export-core\src','qq-chat-export-core\Cargo.toml') | ForEach-Object { Join-Path $root $_ }; $localServer=Join-Path $root 'qq-chat-export-server\target\release\qce-server.exe'; $sourceStale=Newer $localServer $server; $packageStale=Newer $env:QCE_PACKAGE_BUILD_STAMP ($frontend + $plugin + $server + @($localServer)); 'QCE_PACKAGE_STALE=' + [int]$packageStale; 'QCE_SERVER_SOURCE_STALE=' + [int]$sourceStale"`) do set "%%a=%%b"
exit /b 0

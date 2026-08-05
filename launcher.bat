@echo off
chcp 65001 >nul
setlocal EnableExtensions
cd /d "%~dp0"

title QQ Chat Exporter Launcher

if not defined QCE_PACKAGE_DIR set "QCE_PACKAGE_DIR=%~dp0NapCat-QCE-Windows-x64"
set "QCE_PACKAGE_LAUNCHER=%QCE_PACKAGE_DIR%\launcher.bat"

if "%QCE_REBUILD%"=="1" goto :build_package
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
    where cargo >nul 2>&1
    if errorlevel 1 (
        echo [Error] Rust/Cargo was not found and no compiled QCE server is available.
        echo         Build qq-chat-export-server once, or install Rust from https://rustup.rs/.
        goto :failed
    )
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

if "%QCE_BUILD_ONLY%"=="1" (
    echo [QCE] Windows runtime package is ready.
    exit /b 0
)

:launch_package
echo [QCE] Starting "%QCE_PACKAGE_LAUNCHER%"...
call "%QCE_PACKAGE_LAUNCHER%" %*
exit /b %errorlevel%

:failed
echo.
pause
exit /b 1

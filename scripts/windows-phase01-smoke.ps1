param(
    [Parameter(Mandatory = $true)]
    [string]$ExtensionPath
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$extension = (Resolve-Path $ExtensionPath).Path
$extensionForSqlite = $extension.Replace('\', '/')
$targetRoot = if ($env:CARGO_TARGET_DIR) {
    if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $repoRoot $env:CARGO_TARGET_DIR
    }
} else {
    Join-Path $repoRoot "target"
}
$root = Join-Path $targetRoot "phase01\windows-sqlite-3.45.0"
$archive = Join-Path $root "sqlite-autoconf-3450000.tar.gz"
$sourceDir = Join-Path $root "sqlite-autoconf-3450000"
$sqliteExe = Join-Path $root "sqlite3.exe"
$nativeExe = Join-Path $root "native-abi-smoke.exe"
$expectedSha256 = "72887d57a1d8f89f52be38ef84a6353ce8c3ed55ada7864eb944abd9a495e436"

New-Item -ItemType Directory -Force -Path $root | Out-Null

if (-not (Test-Path $archive)) {
    Invoke-WebRequest -Uri "https://www.sqlite.org/2024/sqlite-autoconf-3450000.tar.gz" -OutFile $archive
}

$actualSha256 = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actualSha256 -ne $expectedSha256) {
    throw "SQLite 3.45.0 fixture checksum mismatch"
}

if (-not (Test-Path $sourceDir)) {
    & tar.exe -xzf $archive -C $root
    if ($LASTEXITCODE -ne 0) {
        throw "failed to extract SQLite 3.45.0 fixture"
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere.exe not found"
}
$vcvars = & $vswhere -latest -products "*" -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find "VC\Auxiliary\Build\vcvars64.bat" | Select-Object -First 1
if (-not $vcvars) {
    throw "Visual C++ x64 build environment not found"
}

function Invoke-VcCommand {
    param([Parameter(Mandatory = $true)][string]$Command)

    $commandFile = Join-Path $root "phase01-vc.cmd"
    @(
        "@echo off",
        "call `"$vcvars`"",
        "if errorlevel 1 exit /b %errorlevel%",
        $Command
    ) | Set-Content -Encoding Ascii $commandFile
    & cmd.exe /d /c $commandFile
    if ($LASTEXITCODE -ne 0) {
        throw "Visual C++ command failed: $Command"
    }
}

$sqliteC = Join-Path $sourceDir "sqlite3.c"
$shellC = Join-Path $sourceDir "shell.c"
$sqliteHeaderDir = $sourceDir
$nativeSource = Join-Path $repoRoot "tests\native_abi_smoke.c"
$lithographInclude = Join-Path $repoRoot "include"
$sqliteObj = Join-Path $root "sqlite3-native.obj"
$sqliteLib = Join-Path $root "sqlite3.lib"

Invoke-VcCommand "cd /d `"$root`" && cl /nologo /O2 /DSQLITE_THREADSAFE=1 /DSQLITE_ENABLE_FTS5 `"$sqliteC`" `"$shellC`" /Fe`"$sqliteExe`""
Invoke-VcCommand "cd /d `"$root`" && cl /nologo /c /O2 /w /DSQLITE_THREADSAFE=1 /DSQLITE_ENABLE_FTS5 `"$sqliteC`" /Fo`"$sqliteObj`""
Invoke-VcCommand "cd /d `"$root`" && lib /nologo `"$sqliteObj`" /OUT:`"$sqliteLib`""
$env:LIB = if ($env:LIB) { "$root;$env:LIB" } else { $root }

$version = (& $sqliteExe ":memory:" "SELECT sqlite_version();").Trim()
if ($version -ne "3.45.0") {
    throw "expected SQLite 3.45.0 fixture, got $version"
}

$loadResult = (& $sqliteExe -batch -noheader -cmd ".load `"$extensionForSqlite`"" ":memory:" "SELECT json_valid(lithograph_version());").Trim()
if ($loadResult -ne "1") {
    throw "Windows SQLite failed real Lithograph .load smoke"
}

$env:LITHOGRAPH_SQLITE3 = $sqliteExe
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-sqlite-probe -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows SQLite extension probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase01 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 01 probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase02 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 02 storage probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase03 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 03 frontend probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase04 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 04 read query probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase05 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 05 mutation/transaction probe failed"
}
& cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase06 -- $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Phase 06 Cypher completeness probe failed"
}

$nativeObj = Join-Path $root "native-abi-smoke.obj"
Invoke-VcCommand "cd /d `"$root`" && cl /nologo /c /std:c11 /W4 /WX /I`"$lithographInclude`" /I`"$sqliteHeaderDir`" `"$nativeSource`" /Fo`"$nativeObj`""
Invoke-VcCommand "cd /d `"$root`" && link /nologo `"$nativeObj`" `"$sqliteObj`" /OUT:`"$nativeExe`""
& $nativeExe $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Native C ABI smoke failed"
}

$exportsFile = Join-Path $root "exports.txt"
$importsFile = Join-Path $root "imports.txt"
$dependenciesFile = Join-Path $root "dependencies.txt"
Invoke-VcCommand "dumpbin /nologo /exports `"$extension`" > `"$exportsFile`" && dumpbin /nologo /imports `"$extension`" > `"$importsFile`" && dumpbin /nologo /dependents `"$extension`" > `"$dependenciesFile`""

$exports = Get-Content -Raw $exportsFile
foreach ($symbol in @(
    "sqlite3_lithograph_init",
    "lithograph_v1_execute",
    "lithograph_v1_validate",
    "lithograph_v1_free"
)) {
    if ($exports -notmatch [regex]::Escape($symbol)) {
        throw "Windows artifact is missing required export: $symbol"
    }
}

$imports = Get-Content -Raw $importsFile
if ($imports -match "sqlite3_") {
    throw "Windows extension artifact has direct SQLite imports instead of the host API table"
}
$dependencies = Get-Content -Raw $dependenciesFile
if ($dependencies -match "(?i)sqlite.*\.dll") {
    throw "Windows extension artifact links a private SQLite runtime"
}

Write-Host "Windows Phase 01 load/ABI/artifact smoke passed: $extension"

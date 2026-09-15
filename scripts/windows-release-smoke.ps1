param(
    [Parameter(Mandatory = $true)]
    [string]$ExtensionPath,
    [string]$InteropFixture = ""
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
$root = Join-Path $targetRoot "phase10\windows-release"
New-Item -ItemType Directory -Force -Path $root | Out-Null

$architecture = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$vcvarsName = if ($architecture -eq "arm64") { "vcvarsarm64.bat" } else { "vcvars64.bat" }
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    $vswhere = (Get-Command vswhere.exe -ErrorAction Stop).Source
}
$vcvars = & $vswhere -latest -products "*" -find "VC\Auxiliary\Build\$vcvarsName" | Select-Object -First 1
if (-not $vcvars) {
    throw "Visual C++ $architecture build environment not found"
}

function Invoke-VcCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string]$Name
    )

    $commandFile = Join-Path $root "$Name.cmd"
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

function Build-SqliteRuntime {
    param(
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$ArchiveVersion,
        [Parameter(Mandatory = $true)][string]$Year,
        [Parameter(Mandatory = $true)][string]$Sha256
    )

    $runtimeRoot = Join-Path $root "sqlite-$Version"
    $archive = Join-Path $runtimeRoot "sqlite-autoconf-$ArchiveVersion.tar.gz"
    $sourceDir = Join-Path $runtimeRoot "sqlite-autoconf-$ArchiveVersion"
    $sqliteExe = Join-Path $runtimeRoot "sqlite3.exe"
    $sqliteObj = Join-Path $runtimeRoot "sqlite3-native.obj"
    $sqliteLib = Join-Path $runtimeRoot "sqlite3.lib"
    New-Item -ItemType Directory -Force -Path $runtimeRoot | Out-Null

    if (-not (Test-Path $archive)) {
        Invoke-WebRequest -Uri "https://www.sqlite.org/$Year/sqlite-autoconf-$ArchiveVersion.tar.gz" -OutFile $archive
    }
    $actualSha256 = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $Sha256) {
        throw "SQLite $Version fixture checksum mismatch"
    }
    if (-not (Test-Path $sourceDir)) {
        & tar.exe -xzf $archive -C $runtimeRoot
        if ($LASTEXITCODE -ne 0) {
            throw "failed to extract SQLite $Version fixture"
        }
    }

    $sqliteC = Join-Path $sourceDir "sqlite3.c"
    $shellC = Join-Path $sourceDir "shell.c"
    Invoke-VcCommand -Name "sqlite-$Version-shell" -Command "cd /d `"$runtimeRoot`" && cl /nologo /O2 /DSQLITE_THREADSAFE=1 /DSQLITE_ENABLE_FTS5 `"$sqliteC`" `"$shellC`" /Fe`"$sqliteExe`""
    Invoke-VcCommand -Name "sqlite-$Version-object" -Command "cd /d `"$runtimeRoot`" && cl /nologo /c /O2 /w /DSQLITE_THREADSAFE=1 /DSQLITE_ENABLE_FTS5 `"$sqliteC`" /Fo`"$sqliteObj`""
    Invoke-VcCommand -Name "sqlite-$Version-library" -Command "cd /d `"$runtimeRoot`" && lib /nologo `"$sqliteObj`" /OUT:`"$sqliteLib`""

    $actualVersion = (& $sqliteExe ":memory:" "SELECT sqlite_version();").Trim()
    if ($actualVersion -ne $Version) {
        throw "expected SQLite $Version fixture, got $actualVersion"
    }
    return [PSCustomObject]@{
        Root = $runtimeRoot
        Exe = $sqliteExe
        Obj = $sqliteObj
        Include = $sourceDir
        LibDir = $runtimeRoot
    }
}

function Invoke-LithographProbe {
    param(
        [Parameter(Mandatory = $true)]$Runtime,
        [Parameter(Mandatory = $true)][string]$Probe
    )
    $previousSqlite3 = $env:LITHOGRAPH_SQLITE3
    $previousLibDir = $env:SQLITE3_LIB_DIR
    $previousIncludeDir = $env:SQLITE3_INCLUDE_DIR
    $previousStatic = $env:SQLITE3_STATIC
    try {
        $env:LITHOGRAPH_SQLITE3 = $Runtime.Exe
        $env:SQLITE3_LIB_DIR = $Runtime.LibDir
        $env:SQLITE3_INCLUDE_DIR = $Runtime.Include
        $env:SQLITE3_STATIC = "1"
        & cargo run --locked --quiet -p lithograph-test-support --bin $Probe -- $extension
        if ($LASTEXITCODE -ne 0) {
            throw "$Probe failed with $($Runtime.Exe)"
        }
    } finally {
        $env:LITHOGRAPH_SQLITE3 = $previousSqlite3
        $env:SQLITE3_LIB_DIR = $previousLibDir
        $env:SQLITE3_INCLUDE_DIR = $previousIncludeDir
        $env:SQLITE3_STATIC = $previousStatic
    }
}

function Invoke-Phase09Regressions {
    param(
        [Parameter(Mandatory = $true)]$Runtime
    )
    $previousSqlite3 = $env:LITHOGRAPH_SQLITE3
    $previousLibDir = $env:SQLITE3_LIB_DIR
    $previousIncludeDir = $env:SQLITE3_INCLUDE_DIR
    $previousStatic = $env:SQLITE3_STATIC
    try {
        $env:LITHOGRAPH_SQLITE3 = $Runtime.Exe
        $env:SQLITE3_LIB_DIR = $Runtime.LibDir
        $env:SQLITE3_INCLUDE_DIR = $Runtime.Include
        $env:SQLITE3_STATIC = "1"
        & cargo test --release --locked -p lithograph-core --test phase09_version
        if ($LASTEXITCODE -ne 0) {
            throw "Phase 09 version regressions failed with $($Runtime.Exe)"
        }
    } finally {
        $env:LITHOGRAPH_SQLITE3 = $previousSqlite3
        $env:SQLITE3_LIB_DIR = $previousLibDir
        $env:SQLITE3_INCLUDE_DIR = $previousIncludeDir
        $env:SQLITE3_STATIC = $previousStatic
    }
}

$minimum = Build-SqliteRuntime -Version "3.45.0" -ArchiveVersion "3450000" -Year "2024" -Sha256 "72887d57a1d8f89f52be38ef84a6353ce8c3ed55ada7864eb944abd9a495e436"
$current = Build-SqliteRuntime -Version "3.53.4" -ArchiveVersion "3530400" -Year "2026" -Sha256 "0e9483900e92cd5de8fd48d16bf9200145a61f7fd5be542a5ac81d8a9516eb9c"

Invoke-Phase09Regressions -Runtime $minimum

foreach ($runtime in @($minimum, $current)) {
    $loadResult = (& $runtime.Exe -batch -noheader -cmd ".load `"$extensionForSqlite`"" ":memory:" "SELECT json_valid(lithograph_version());").Trim()
    if ($loadResult -ne "1") {
        throw "SQLite runtime failed real Lithograph .load smoke: $($runtime.Exe)"
    }
}

foreach ($probe in @(
    "lithograph-sqlite-probe",
    "lithograph-phase01",
    "lithograph-phase02",
    "lithograph-phase03",
    "lithograph-phase04",
    "lithograph-phase05",
    "lithograph-phase06",
    "lithograph-phase07",
    "lithograph-phase08",
    "lithograph-phase09"
)) {
    Invoke-LithographProbe -Runtime $minimum -Probe $probe
}
foreach ($probe in @(
    "lithograph-sqlite-probe",
    "lithograph-phase01",
    "lithograph-phase04",
    "lithograph-phase05"
)) {
    Invoke-LithographProbe -Runtime $current -Probe $probe
}

if ($InteropFixture) {
    $fixture = (Resolve-Path $InteropFixture).Path
    $previousSqlite3 = $env:LITHOGRAPH_SQLITE3
    $previousLibDir = $env:SQLITE3_LIB_DIR
    $previousIncludeDir = $env:SQLITE3_INCLUDE_DIR
    $previousStatic = $env:SQLITE3_STATIC
    try {
        $env:LITHOGRAPH_SQLITE3 = $current.Exe
        $env:SQLITE3_LIB_DIR = $current.LibDir
        $env:SQLITE3_INCLUDE_DIR = $current.Include
        $env:SQLITE3_STATIC = "1"
        & cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase10-storage-fixture -- verify $fixture $extension
        if ($LASTEXITCODE -ne 0) {
            throw "cross-platform storage interoperability fixture failed"
        }
    } finally {
        $env:LITHOGRAPH_SQLITE3 = $previousSqlite3
        $env:SQLITE3_LIB_DIR = $previousLibDir
        $env:SQLITE3_INCLUDE_DIR = $previousIncludeDir
        $env:SQLITE3_STATIC = $previousStatic
    }
}

$nativeSource = Join-Path $repoRoot "tests\native_abi_smoke.c"
$lithographInclude = Join-Path $repoRoot "include"
$nativeObj = Join-Path $root "native-abi-smoke.obj"
$nativeExe = Join-Path $root "native-abi-smoke.exe"
Invoke-VcCommand -Name "native-compile" -Command "cd /d `"$root`" && cl /nologo /c /std:c11 /W4 /WX /I`"$lithographInclude`" /I`"$($minimum.Include)`" `"$nativeSource`" /Fo`"$nativeObj`""
Invoke-VcCommand -Name "native-link" -Command "cd /d `"$root`" && link /nologo `"$nativeObj`" `"$($minimum.Obj)`" /OUT:`"$nativeExe`""
& $nativeExe $extension
if ($LASTEXITCODE -ne 0) {
    throw "Windows Native C ABI smoke failed"
}

$exportsFile = Join-Path $root "exports.txt"
$importsFile = Join-Path $root "imports.txt"
$dependenciesFile = Join-Path $root "dependencies.txt"
Invoke-VcCommand -Name "artifact-inspect" -Command "dumpbin /nologo /exports `"$extension`" > `"$exportsFile`" && dumpbin /nologo /imports `"$extension`" > `"$importsFile`" && dumpbin /nologo /dependents `"$extension`" > `"$dependenciesFile`""

$exports = Get-Content -Raw $exportsFile
foreach ($symbol in @(
    "sqlite3_lithograph_init",
    "lithograph_v1_execute",
    "lithograph_v1_validate",
    "lithograph_v1_tx_begin",
    "lithograph_v1_tx_execute",
    "lithograph_v1_tx_commit",
    "lithograph_v1_tx_abort",
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

Write-Host "Windows Phase 10 release artifact smoke passed ($architecture): $extension"

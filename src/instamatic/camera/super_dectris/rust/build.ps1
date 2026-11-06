# build.ps1 - build and install the _super_dectris extension (docs/DESIGN.md section 7).
#
#   powershell -ExecutionPolicy Bypass -File src\instamatic\camera\super_dectris\rust\build.ps1 -Python <path to python.exe> [-Quick] [-NoChecks]
#
#   (default)  test + perf bounds (test-opt) + clippy + deny + release build + copy .pyd + import assertion
#              + cargo about notices + _build_info.json + PDB copy
#   -Quick     skip the post-build extras (cargo about, _build_info.json/dumpbin, PDB copy).
#              The copy to the .pyd and the import assertion always run: they ARE the install.
#   -NoChecks  skip the pre-build checks (cargo test, perf bounds, clippy, cargo deny).
#
# cargo-deny / cargo-about missing -> warning, step skipped (install: cargo install --locked cargo-deny cargo-about).
# Windows PowerShell 5.1 compatible (no &&, native exit codes checked by hand). ASCII only.
param(
    [string]$Python = '',   # never PATH `python` (WindowsApps stub)
    [switch]$Quick,
    [switch]$NoChecks
)
$ErrorActionPreference = 'Stop'

$RustDir = $PSScriptRoot
$Repo = (Resolve-Path (Join-Path $RustDir '..\..\..\..\..')).Path   # backslash form, for --remap-path-prefix
$PkgDir = Join-Path $Repo 'src\instamatic\camera\super_dectris'
$Pyd = Join-Path $PkgDir '_super_dectris.cp312-win_amd64.pyd'
$Addendum = Join-Path $RustDir 'notices_addendum.md'
# MSVC's linker fails on long paths (LNK1104), even with Windows long paths enabled.
if ($Repo.Length -gt 150) {
    throw "The repository path is $($Repo.Length) characters long; MSVC's linker fails on long paths. Clone into a short path such as C:\src."
}

if (-not $Python -and $env:CONDA_PREFIX) { $Python = Join-Path $env:CONDA_PREFIX 'python.exe' }   # else the active conda env
if (-not $Python) { throw 'Python not given (pass -Python <path to python.exe>)' }
if (-not (Test-Path $Python)) { throw "Python not found: $Python (pass -Python <path to python.exe>)" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo not found (install rustup)' }
}

$env:PYO3_PYTHON = $Python                                    # same value for every cargo call -> no pyo3 rebuild churn [R27]
$env:CARGO_TARGET_DIR = Join-Path $Repo 'build\cargo-target'  # outside the importable package [R36]
# [R41] reproducible paths in the artefact. NOTE: a cargo call with different RUSTFLAGS (e.g. a plain
# shell) rebuilds everything in the same target dir.
$env:RUSTFLAGS = "--remap-path-prefix=$Repo=. --remap-path-prefix=$env:USERPROFILE\.cargo=cargo -C link-arg=/PDBALTPATH:%_PDB%"
# The same for libzmq's C++, whose __FILE__ paths stay in the binary. cc splits these flags at spaces.
if ($env:USERPROFILE -notmatch ' ') {
    $env:CFLAGS = "/d1trimfile:$env:USERPROFILE\.cargo\"
    $env:CXXFLAGS = $env:CFLAGS
}

function Invoke-Native {
    param([string]$Name, [scriptblock]$Block)
    Write-Host "==> $Name" -ForegroundColor Cyan
    # PS 5.1 may wrap native stderr (cargo progress) as NativeCommandError, which 'Stop' would turn
    # into a terminating error; judge success by the exit code only.
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Block } finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE -ne 0) { throw "$Name failed (exit code $LASTEXITCODE)" }
}

function Test-Tool([string]$Exe) {
    return [bool](Get-Command $Exe -ErrorAction SilentlyContinue)
}

Push-Location $RustDir   # rust-toolchain.toml (1.91.1) and .cargo/config.toml apply here
try {
    if (-not $NoChecks) {
        Invoke-Native 'cargo test --workspace' { cargo test --workspace }
        Invoke-Native 'perf bounds (test-opt)' { cargo test -p dectris_codec --profile test-opt --test perf }   # bounds asserted only when optimised
        Invoke-Native 'cargo clippy' { cargo clippy --workspace --all-targets -- -D warnings }
        if (Test-Tool 'cargo-deny') {
            Invoke-Native 'cargo deny check licenses' { cargo deny check licenses }
        } else {
            Write-Warning 'cargo-deny not installed: licence check SKIPPED (cargo install --locked cargo-deny)'
        }
    }

    Invoke-Native 'cargo build --release -p super_dectris_py' { cargo build --release -p super_dectris_py }

    $Dll = Join-Path $env:CARGO_TARGET_DIR 'release\_super_dectris.dll'
    if (-not (Test-Path $Dll)) { throw "build output missing: $Dll" }
    Write-Host "==> copy $Dll -> $Pyd" -ForegroundColor Cyan
    Copy-Item -Force $Dll $Pyd
} finally {
    Pop-Location
}

Push-Location $Repo   # the editable install resolves instamatic from src/
try {
    $check = "import pathlib, instamatic.camera.super_dectris._super_dectris as m; " +
             "assert pathlib.Path(m.__file__).parent.name == 'super_dectris', m.__file__; print(m.__version__)"
    Invoke-Native 'import assertion' { & $Python -I -c $check }
} finally {
    Pop-Location
}

if ($Quick) {
    Write-Host 'Done (-Quick: skipped cargo about, _build_info.json, PDB copy).' -ForegroundColor Green
    return
}

Push-Location $RustDir
try {
    $Notices = Join-Path $PkgDir 'THIRD_PARTY_NOTICES.md'
    if (Test-Tool 'cargo-about') {
        Invoke-Native 'cargo about generate' { cargo about generate about.hbs -o $Notices }
        if (Test-Path $Addendum) {
            $utf8NoBom = New-Object System.Text.UTF8Encoding $false
            [System.IO.File]::AppendAllText($Notices, [System.IO.File]::ReadAllText($Addendum, $utf8NoBom), $utf8NoBom)
        }
    } else {
        Write-Warning 'cargo-about not installed: THIRD_PARTY_NOTICES.md NOT regenerated (cargo install --locked cargo-about)'
    }

    # _build_info.json [R29]: rustc -V, the driver's version, git rev, sha256(.pyd), dumpbin /DEPENDENTS.
    # Written to build\ (gitignored). git_rev and git_dirty describe the repository the driver sits in.
    $BuildDir = Join-Path $Repo 'build'
    New-Item -ItemType Directory -Force $BuildDir | Out-Null
    $rustc = (& rustc -V) -join ''
    $driverVersion = (Select-String -Path (Join-Path $RustDir 'Cargo.toml') -Pattern '^version = "(.+)"$' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    $gitRev = (& git -C $Repo rev-parse HEAD) -join ''
    $gitDirty = [bool]((& git -C $Repo status --porcelain --untracked-files=no) -join '')
    $sha = (Get-FileHash -Algorithm SHA256 $Pyd).Hash.ToLower()
    $dependents = 'dumpbin not found'
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $dumpbin = $null
    if (Test-Tool 'dumpbin') {
        $dumpbin = (Get-Command dumpbin).Source
    } elseif (Test-Path $vswhere) {
        $dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
    }
    if ($dumpbin -and (Test-Path $dumpbin)) {
        $dependents = (& $dumpbin /NOLOGO /DEPENDENTS $Pyd) -join "`n"
    }
    $info = [ordered]@{
        rustc            = $rustc
        driver_version   = $driverVersion
        git_rev          = $gitRev
        git_dirty        = $gitDirty
        pyd              = '_super_dectris.cp312-win_amd64.pyd'
        sha256           = $sha
        dumpbin_dependents = $dependents
        built_utc        = (Get-Date).ToUniversalTime().ToString('o')
    }
    $info | ConvertTo-Json | Out-File -Encoding utf8 (Join-Path $BuildDir '_build_info.json')

    $Pdb = Join-Path $env:CARGO_TARGET_DIR 'release\_super_dectris.pdb'
    if (Test-Path $Pdb) { Copy-Item -Force $Pdb (Join-Path $BuildDir '_super_dectris.pdb') }
} finally {
    Pop-Location
}
Write-Host "Done. sha256(.pyd) = $sha; build info in build\_build_info.json" -ForegroundColor Green

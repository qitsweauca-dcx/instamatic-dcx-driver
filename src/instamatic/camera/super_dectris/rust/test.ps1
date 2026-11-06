# test.ps1 - run the Rust tests with the environment of build.ps1 (docs/DESIGN.md section 8).
#
#   powershell -ExecutionPolicy Bypass -File src\instamatic\camera\super_dectris\rust\test.ps1 -Python <path to python.exe> [cargo test arguments]
#
# A cargo call with other PYO3_PYTHON, CARGO_TARGET_DIR or RUSTFLAGS than build.ps1's rebuilds every
# crate; this script sets the same values, so it reuses build.ps1's build. Examples:
#   test.ps1 -Python C:\venv\Scripts\python.exe
#   test.ps1 -Python C:\venv\Scripts\python.exe --package dectris_codec --test framing_vectors
# Give cargo's long options (--package, not -p): PowerShell reads a single-dash option as a
# parameter of this script.
# Set DECTRIS_TEST_PYTHON (a Python with dectris-compression and hdf5plugin) to run the oracle tests,
# and DECTRIS_MONITOR_SAMPLE (a Monitor TIFF saved from a DCU) for one more TIFF test; without them
# those tests print SKIP and pass. Windows PowerShell 5.1 compatible. ASCII only.
param(
    [string]$Python = '',
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

$RustDir = $PSScriptRoot
$Repo = (Resolve-Path (Join-Path $RustDir '..\..\..\..\..')).Path

if (-not $Python -and $env:CONDA_PREFIX) { $Python = Join-Path $env:CONDA_PREFIX 'python.exe' }
if (-not $Python) { throw 'Python not given (pass -Python <path to python.exe>)' }
if (-not (Test-Path $Python)) { throw "Python not found: $Python" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH" }

# Keep these three in step with build.ps1
$env:PYO3_PYTHON = $Python
$env:CARGO_TARGET_DIR = Join-Path $Repo 'build\cargo-target'
$env:RUSTFLAGS = "--remap-path-prefix=$Repo=. --remap-path-prefix=$env:USERPROFILE\.cargo=cargo -C link-arg=/PDBALTPATH:%_PDB%"
if ($env:USERPROFILE -notmatch ' ') {
    $env:CFLAGS = "/d1trimfile:$env:USERPROFILE\.cargo\"
    $env:CXXFLAGS = $env:CFLAGS
}

Push-Location $RustDir
try {
    if (-not $CargoArgs) { $CargoArgs = @('--workspace') }
    & cargo test @CargoArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}

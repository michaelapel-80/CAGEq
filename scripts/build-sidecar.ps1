# Freeze the Python DSP sidecar into a self-contained bundle for Tauri to ship.
#
# Output: cageq-app\src-tauri\sidecar\cageq-sidecar\  (gitignored, ~177 MB)
# Run this BEFORE `npm run tauri build` whenever the sidecar or its deps change.
#
# Requires the Python 3.10 venv at cageq-sidecar\.venv (see cageq-sidecar\python\requirements.txt).

$ErrorActionPreference = "Stop"
$sidecar = Join-Path $PSScriptRoot "..\cageq-sidecar"
$py = Join-Path $sidecar ".venv\Scripts\python.exe"

if (-not (Test-Path $py)) {
    Write-Error "No venv at $py. Set it up first:  py -3.10 -m venv .venv; .\.venv\Scripts\python -m pip install -r python\requirements.txt"
}

& $py -m pip install --quiet pyinstaller
& $py -m PyInstaller --noconfirm --onedir --console --name cageq-sidecar `
    --collect-all autoeq `
    --distpath (Join-Path $sidecar "..\cageq-app\src-tauri\sidecar") `
    --workpath (Join-Path $sidecar "build") `
    --specpath (Join-Path $sidecar "build") `
    (Join-Path $sidecar "python\sidecar_dsp.py")

Write-Host "Frozen sidecar -> cageq-app\src-tauri\sidecar\cageq-sidecar\  (now run: cd cageq-app; npm run tauri build)"

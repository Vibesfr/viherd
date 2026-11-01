# Viherd — construit l'installeur
#
#   .\build-installer.ps1
#
# 1. compile viherd.exe et viherd-app.exe en release
# 2. compile viherd-setup.exe, qui les embarque (interface : installer\src\ui.html)
# Résultat : target\release\viherd-setup.exe

$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
Push-Location $root
try {
    Write-Host 'Compilation de Viherd (release)…' -ForegroundColor Cyan
    cmd /c "cargo build --release --bin viherd --bin viherd-app 2>&1"
    if ($LASTEXITCODE -ne 0) { Write-Host 'Échec de la compilation de Viherd' -ForegroundColor Red; exit 1 }

    Write-Host 'Compilation de l''installeur…' -ForegroundColor Cyan
    cmd /c "cargo build --release -p viherd-setup 2>&1"
    if ($LASTEXITCODE -ne 0) { Write-Host 'Échec de la compilation de l''installeur' -ForegroundColor Red; exit 1 }

    $out = Join-Path $root 'target\release\viherd-setup.exe'
    $mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
    Write-Host "✔ $out ($mb Mo)" -ForegroundColor Green
}
finally { Pop-Location }

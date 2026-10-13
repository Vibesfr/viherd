# Viherd — mode développement
#
#   .\dev.ps1
#
# - Lance l'appli avec VIHERD_DEV=1 : src/ui.html est lu depuis le disque et l'interface
#   se recharge toute seule à chaque enregistrement (aucune recompilation pour l'UI).
# - Surveille src/*.rs et Cargo.toml : à chaque modification, arrête proprement l'appli
#   (bases de données comprises), recompile, puis la relance.
# Ctrl+C pour sortir (l'appli est arrêtée proprement).

# Pas de 'Stop' : cargo écrit sa progression sur stderr, que Windows PowerShell 5.1
# prendrait pour des erreurs fatales.
$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
$exe = Join-Path $root 'target\debug\viherd-app.exe'

# port de l'API (config.toml), 7777 par défaut
$apiPort = 7777
$cfg = Join-Path $env:USERPROFILE '.viherd\config.toml'
if (Test-Path $cfg) {
    $m = Select-String -Path $cfg -Pattern '^\s*api_port\s*=\s*(\d+)' | Select-Object -First 1
    if ($m) { $apiPort = [int]$m.Matches[0].Groups[1].Value }
}
$api = "http://127.0.0.1:$apiPort"

function Write-Step($msg, $color = 'Cyan') { Write-Host ("[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss'), $msg) -ForegroundColor $color }

function Stop-App {
    $procs = @(Get-Process viherd-app -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })
    if (-not $procs.Count) { return }
    Write-Step 'Arrêt propre de Viherd…' 'DarkGray'
    try { Invoke-WebRequest -UseBasicParsing -Method POST "$api/api/quit" -Headers @{ 'X-Viherd' = '1' } -TimeoutSec 5 | Out-Null } catch {}
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline -and @(Get-Process viherd-app -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }).Count) { Start-Sleep -Milliseconds 300 }
    # dernier recours : tout l'arbre (php-cgi, workers, services lancés par l'appli)
    Get-Process viherd-app -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | ForEach-Object { taskkill.exe /PID $_.Id /T /F | Out-Null }
}

function Build {
    Write-Step 'Compilation…'
    Push-Location $root
    try {
        # via cmd : stdout + stderr fusionnés en texte simple, sans « NativeCommandError »
        $out = cmd /c "cargo build --color never --bin viherd-app --bin viherd 2>&1"
        $code = $LASTEXITCODE
        $out | Where-Object { $_ -match '^(error|warning)|^\s+-->|^\s+\||Finished' } | ForEach-Object { Write-Host "  $_" }
    }
    finally { Pop-Location }
    return ($code -eq 0)
}

function Start-App {
    $env:VIHERD_DEV = '1'
    $env:VIHERD_UI = Join-Path $root 'src\ui.html'
    Start-Process $exe | Out-Null
    Write-Step 'Viherd lancé en mode dev (l''UI se recharge toute seule)' 'Green'
}

# --- la version normale (release) occupe les mêmes ports : il faut la quitter d'abord
$other = @(Get-Process viherd-app, viherd -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -notlike "$root\target\debug\*" })
if ($other.Count) {
    Write-Step "Une autre instance de Viherd tourne déjà ($($other[0].Path))." 'Red'
    Write-Step 'Quitte-la (clic droit sur l''icône Viherd > Quitter), puis relance .\dev.ps1' 'Red'
    exit 1
}

# --- premier lancement
Stop-App
if (Build) { Start-App } else { Write-Step 'La compilation a échoué : corrige puis enregistre un fichier .rs' 'Red' }

# --- surveillance du code Rust
# Vérification de la date des fichiers chaque seconde : plus fiable que FileSystemWatcher,
# qui peut rater des enregistrements (écritures atomiques des éditeurs, rafales).
function Get-SourceStamp {
    $files = @(Get-ChildItem (Join-Path $root 'src') -Recurse -File -Include *.rs, *.php -ErrorAction SilentlyContinue) + @(Get-Item (Join-Path $root 'Cargo.toml'))
    $latest = $files | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    [pscustomobject]@{ Ticks = $latest.LastWriteTimeUtc.Ticks; Name = $latest.Name }
}
$stamp = (Get-SourceStamp).Ticks
Write-Step "Surveillance de src\ (*.rs, *.php) et Cargo.toml — Ctrl+C pour quitter" 'DarkGray'

try {
    while ($true) {
        Start-Sleep -Milliseconds 1000
        $now = Get-SourceStamp
        if ($now.Ticks -eq $stamp) { continue }
        Start-Sleep -Milliseconds 400   # laisse l'éditeur finir d'écrire (enregistrements multiples)
        $now = Get-SourceStamp
        $stamp = $now.Ticks
        Write-Step "Modifié : $($now.Name)" 'Yellow'
        Stop-App
        if (Build) { Start-App } else { Write-Step 'Compilation échouée : l''appli reste arrêtée jusqu''à la prochaine correction' 'Red' }
        $stamp = (Get-SourceStamp).Ticks   # ignore ce qui a changé pendant la compilation, déjà inclus
    }
}
finally {
    Stop-App
    Write-Step 'Mode dev terminé.' 'DarkGray'
}

param(
    [string]$InnoCompiler = 'ISCC.exe',
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$compiler = Get-Command -Name $InnoCompiler -CommandType Application
$target = 'x86_64-pc-windows-msvc'
$pawnIoVersion = '2.2.0'
$pawnIoHash = '1F519A22E47187F70A1379A48CA604981C4FCF694F4E65B734AAA74A9FBA3032'
if ($OutputDirectory) {
    $OutputDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    cargo build --release --locked --target $target --target-dir target
    if ($LASTEXITCODE -ne 0) {
        throw 'Falha ao compilar o ARGOS.'
    }

    $version = & ".\target\$target\release\argos.exe" --version
    if ($LASTEXITCODE -ne 0 -or $version -notmatch '^ARGOS (\S+)$') {
        throw 'Falha ao ler a versao do ARGOS.'
    }

    $appVersion = $Matches[1]
    $sensorDirectory = Join-Path (Get-Location).Path 'target\package-tools\argos-sensor'
    New-Item -ItemType Directory -Path $sensorDirectory -Force | Out-Null
    $pawnIo = Join-Path $sensorDirectory "PawnIO-$pawnIoVersion.exe"
    if (!(Test-Path -LiteralPath $pawnIo -PathType Leaf)) {
        $download = "$pawnIo.$([guid]::NewGuid().ToString('N')).download"
        try {
            $url = "https://github.com/namazso/PawnIO.Setup/releases/download/$pawnIoVersion/PawnIO_setup.exe"
            Invoke-WebRequest -Uri $url -OutFile $download
            if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ne $pawnIoHash) {
                throw 'O instalador do PawnIO não corresponde ao arquivo oficial esperado.'
            }
            Move-Item -LiteralPath $download -Destination $pawnIo -Force
        }
        finally {
            if (Test-Path -LiteralPath $download) {
                Remove-Item -LiteralPath $download -ErrorAction SilentlyContinue
            }
        }
    }
    if ((Get-FileHash -LiteralPath $pawnIo -Algorithm SHA256).Hash -ne $pawnIoHash) {
        throw 'O instalador do PawnIO em cache não corresponde ao arquivo oficial esperado.'
    }

    & $compiler.Source "/DAppVersion=$appVersion" "/DPawnIoInstaller=$pawnIo" `
        "/DPawnIoSha256=$pawnIoHash" "/O$sensorDirectory" (Join-Path $PSScriptRoot 'argos-sensor.iss')
    if ($LASTEXITCODE -ne 0) {
        throw 'Falha ao gerar o instalador do sensor do ARGOS.'
    }

    $sensorInstaller = Join-Path $sensorDirectory 'ARGOS-Sensor-Setup.exe'
    $sensorHash = (Get-FileHash -LiteralPath $sensorInstaller -Algorithm SHA256).Hash
    $compilerOptions = @("/DAppVersion=$appVersion", "/DSensorInstaller=$sensorInstaller", "/DSensorSha256=$sensorHash")
    if ($OutputDirectory) { $compilerOptions += "/O$OutputDirectory" }
    & $compiler.Source @compilerOptions (Join-Path $PSScriptRoot 'argos.iss')
    if ($LASTEXITCODE -ne 0) {
        throw 'Falha ao gerar o instalador do ARGOS.'
    }
}
finally {
    Pop-Location
}

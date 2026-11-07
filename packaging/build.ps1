param([string]$InnoCompiler = 'ISCC.exe')

$ErrorActionPreference = 'Stop'
$compiler = Get-Command -Name $InnoCompiler -CommandType Application
$target = 'x86_64-pc-windows-msvc'

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

    & $compiler.Source "/DAppVersion=$($Matches[1])" (Join-Path $PSScriptRoot 'argos.iss')
    if ($LASTEXITCODE -ne 0) {
        throw 'Falha ao gerar o instalador do ARGOS.'
    }
}
finally {
    Pop-Location
}

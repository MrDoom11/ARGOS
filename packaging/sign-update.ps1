[CmdletBinding(DefaultParameterSetName = 'Sign')]
param(
    [Parameter(Mandatory)]
    [string]$PrivateKey,

    [string]$OpenSsl = 'openssl.exe',

    [Parameter(Mandatory, ParameterSetName = 'PublicKey')]
    [switch]$PublicKey,

    [Parameter(Mandatory, ParameterSetName = 'Sign')]
    [string]$Installer,

    [Parameter(Mandatory, ParameterSetName = 'Sign')]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$opensslExe = (Get-Command -Name $OpenSsl -CommandType Application).Source
$keyPath = (Resolve-Path -LiteralPath $PrivateKey).ProviderPath
$utf8 = [System.Text.UTF8Encoding]::new($false)

function ConvertTo-LowerHex([byte[]]$Bytes) {
    [BitConverter]::ToString($Bytes).Replace('-', '').ToLowerInvariant()
}

if ($PSCmdlet.ParameterSetName -eq 'Sign') {
    $numeric = '(?:0|[1-9][0-9]*)'
    $pre = "(?:$numeric|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    $pattern = "\A($numeric)\.($numeric)\.($numeric)(?:-$pre(?:\.$pre)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z"
    if ($Version.Length -gt 255 -or $Version -cnotmatch $pattern) {
        throw 'Informe uma versão SemVer válida, como 0.2.0.'
    }
    [UInt64]$part = 0
    foreach ($index in 1..3) {
        if (-not [UInt64]::TryParse($Matches[$index], [ref]$part)) {
            throw 'Um componente da versão excede o limite permitido.'
        }
    }
}

$scratch = Join-Path ([IO.Path]::GetTempPath()) ('argos-sign-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($scratch) | Out-Null
$publicDer = Join-Path $scratch 'public.der'
$messagePath = Join-Path $scratch 'manifest.txt'
$signaturePath = Join-Path $scratch 'signature.bin'
$pendingManifest = $null
try {
    & $opensslExe pkey -in $keyPath -pubout -outform DER -out $publicDer
    if ($LASTEXITCODE -ne 0) {
        throw 'O OpenSSL não conseguiu ler a chave privada.'
    }
    $spkiPrefix = '302a300506032b6570032100'
    $derHex = ConvertTo-LowerHex ([IO.File]::ReadAllBytes($publicDer))
    if ($derHex.Length -ne ($spkiPrefix.Length + 64) -or -not $derHex.StartsWith($spkiPrefix)) {
        throw 'A chave privada precisa ser Ed25519.'
    }
    if ($PublicKey) {
        return $derHex.Substring($spkiPrefix.Length)
    }

    $installerPath = (Resolve-Path -LiteralPath $Installer).ProviderPath
    $stream = [IO.File]::Open($installerPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $size = $stream.Length
        if ($size -lt 1 -or $size -gt 32MB) {
            throw 'O instalador precisa ter entre 1 byte e 32 MiB.'
        }
        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $sha256 = ConvertTo-LowerHex ($hasher.ComputeHash($stream))
        }
        finally {
            $hasher.Dispose()
        }

        $sizeText = $size.ToString([Globalization.CultureInfo]::InvariantCulture)
        $signedText = "ARGOS-UPDATE-v1`n$Version`n$sizeText`n$sha256`n"
        [IO.File]::WriteAllText($messagePath, $signedText, $utf8)
        & $opensslExe pkeyutl -sign -rawin -inkey $keyPath -in $messagePath -out $signaturePath
        if ($LASTEXITCODE -ne 0) {
            throw 'O OpenSSL não conseguiu assinar o manifesto.'
        }
        $signature = ConvertTo-LowerHex ([IO.File]::ReadAllBytes($signaturePath))
        if ($signature.Length -ne 128) {
            throw 'O OpenSSL gerou uma assinatura Ed25519 inválida.'
        }

        $manifest = [ordered]@{
            version = $Version
            size = $size
            sha256 = $sha256
            signature = $signature
        } | ConvertTo-Json -Compress
        $directory = [IO.Path]::GetDirectoryName($installerPath)
        $manifestPath = Join-Path $directory 'argos-update.json'
        $pendingManifest = Join-Path $directory ('argos-update-' + [guid]::NewGuid().ToString('N') + '.tmp')
        [IO.File]::WriteAllText($pendingManifest, "$manifest`n", $utf8)
        if ([IO.File]::Exists($manifestPath)) {
            [IO.File]::Replace($pendingManifest, $manifestPath, [NullString]::Value)
        }
        else {
            [IO.File]::Move($pendingManifest, $manifestPath)
        }
        $manifestPath
    }
    finally {
        $stream.Dispose()
    }
}
finally {
    foreach ($path in @($publicDer, $messagePath, $signaturePath, $pendingManifest)) {
        if ($path -and [IO.File]::Exists($path)) {
            [IO.File]::Delete($path)
        }
    }
    [IO.Directory]::Delete($scratch, $false)
}

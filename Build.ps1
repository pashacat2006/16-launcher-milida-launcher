param([switch]$CheckOnly)
$ErrorActionPreference = 'Stop'
$taskRoot = $PSScriptRoot
$taskCargo = Join-Path $taskRoot '.tools/toolchain/bin/cargo.exe'
if (!(Test-Path -LiteralPath $taskCargo)) { throw 'В папке .tools/toolchain нет подготовленного компилятора Rust.' }
$env:CARGO_HOME = Join-Path $taskRoot '.tools/cargo'
$env:RUSTC = Join-Path $taskRoot '.tools/toolchain/bin/rustc.exe'
$env:RUSTDOC = Join-Path $taskRoot '.tools/toolchain/bin/rustdoc.exe'
$env:MEDIA_REMOTE = '1'
$env:CARGO_HTTP_TIMEOUT = '120'
$env:CARGO_HTTP_MULTIPLEXING = 'false'
Push-Location (Join-Path $taskRoot 'hybrid')
try {
    if (!(Test-Path -LiteralPath node_modules)) {
        & npm.cmd ci --ignore-scripts --no-audit --no-fund --cache (Join-Path $taskRoot '.tools/npm-cache')
        if ($LASTEXITCODE -ne 0) { throw 'Не удалось установить зависимости интерфейса' }
    }
    & npm.cmd run build:web
    if ($LASTEXITCODE -ne 0) { throw 'Проверка интерфейса не прошла' }
    if ($CheckOnly) {
        & $taskCargo check --locked --manifest-path src-tauri/Cargo.toml --lib -j 6
    } else {
        & $taskCargo build --locked --manifest-path src-tauri/Cargo.toml --profile fast --features tauri/custom-protocol --bin millida-launcher -j 6
    }
    if ($LASTEXITCODE -ne 0) { throw 'Проверка или сборка Rust не прошла' }
    if (!$CheckOnly) {
        $taskOutput = Join-Path $taskRoot 'Launcher'
        New-Item -ItemType Directory -Force -Path $taskOutput | Out-Null
        Copy-Item -LiteralPath 'src-tauri/target/fast/millida-launcher.exe' -Destination (Join-Path $taskOutput 'Millida16.exe')
        Write-Output ('Готово: ' + (Join-Path $taskOutput 'Millida16.exe'))
    }
} finally { Pop-Location }

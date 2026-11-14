# Fast optimized build of the app (thin LTO). Output: target\fast\noisegate.exe
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location $PSScriptRoot
cargo build --profile fast -p noisegate 2>&1 | Tee-Object -FilePath "$PSScriptRoot\target\app-build.log"

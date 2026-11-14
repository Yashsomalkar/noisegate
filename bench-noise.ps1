# Runs the denoiser bench once per noise type (white, clicks).
# Usage: .\bench-noise.ps1 <clean-48k-mono-16bit.wav>
param([Parameter(Mandatory)][string]$Wav)
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location $PSScriptRoot
cargo build --profile fast -p noisegate-dsp --example bench
foreach ($k in "white", "clicks") {
    $env:NOISE = $k
    & .\target\fast\examples\bench.exe $Wav 2>&1 | Tee-Object -FilePath "$PSScriptRoot\target\bench-$k.log"
}

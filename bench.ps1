# Builds with the fast profile and runs the denoiser bench.
# Usage: .\bench.ps1 <clean-48k-mono-16bit.wav> [out_dir]
param([Parameter(Mandatory)][string]$Wav, [string]$OutDir = "..\denoise-samples")
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location $PSScriptRoot
cargo run --profile fast -p noisegate-dsp --example bench -- $Wav $OutDir 2>&1 | Tee-Object -FilePath "$PSScriptRoot\target\bench.log"

# Builds, renders the tray icons, and makes enlarged PNG previews.
# Output: ..\icon-preview\{on,bypass,problem}.png (32px shown at 8x) and
# strip.png (all three at 1x and 4x, on light and dark backgrounds).
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location $PSScriptRoot
cargo build --profile fast -p noisegate 2>&1 | Select-String "^error|Finished" -Context 0,6
$out = Join-Path $PSScriptRoot "..\icon-preview"
& .\target\fast\noisegate.exe --dump-icons $out
foreach ($n in "on", "bypass", "problem") {
    ffmpeg -hide_banner -loglevel error -y -f rawvideo -pix_fmt rgba -s 32x32 -i "$out\$n.rgba" -vf "scale=256:256:flags=neighbor" "$out\$n.png"
}
# Strip: each icon at 4x over a light and a dark tile, plus 1x (true size).
ffmpeg -hide_banner -loglevel error -y `
    -f rawvideo -pix_fmt rgba -s 32x32 -i "$out\on.rgba" `
    -f rawvideo -pix_fmt rgba -s 32x32 -i "$out\bypass.rgba" `
    -f rawvideo -pix_fmt rgba -s 32x32 -i "$out\problem.rgba" `
    -filter_complex "color=c=0xf3f3f3:s=432x272[l];color=c=0x202020:s=432x272[d];[0]scale=128:128:flags=lanczos[a];[1]scale=128:128:flags=lanczos[b];[2]scale=128:128:flags=lanczos[c];[0]scale=128:128:flags=lanczos[a2];[1]scale=128:128:flags=lanczos[b2];[2]scale=128:128:flags=lanczos[c2];[l][a]overlay=8:8[l1];[l1][b]overlay=152:8[l2];[l2][c]overlay=296:8[l3];[l3][0]overlay=56:180[l4];[l4][1]overlay=200:180[l5];[l5][2]overlay=344:180[lt];[d][a2]overlay=8:8[d1];[d1][b2]overlay=152:8[d2];[d2][c2]overlay=296:8[d3];[d3][0]overlay=56:180[d4];[d4][1]overlay=200:180[d5];[d5][2]overlay=344:180[dt];[lt][dt]vstack" `
    -frames:v 1 "$out\strip.png"
# Transitions: one row of 8 frames (4x, on a light tile) per state pair.
$rows = @()
foreach ($pair in "bypass-on", "on-bypass", "on-problem", "problem-on") {
    $bytes = foreach ($i in 0..7) { [IO.File]::ReadAllBytes("$out\anim-$pair-$i.rgba") }
    [IO.File]::WriteAllBytes("$out\anim-$pair.raw", [byte[]]$bytes)
    ffmpeg -hide_banner -loglevel error -y -f rawvideo -pix_fmt rgba -s 32x32 -i "$out\anim-$pair.raw" `
        -vf "scale=128:128:flags=lanczos,pad=144:144:8:8:color=0xf3f3f3,tile=8x1" -frames:v 1 "$out\anim-$pair.png"
    $rows += "$out\anim-$pair.png"
}
$inputs = $rows | ForEach-Object { "-i"; $_ }
ffmpeg -hide_banner -loglevel error -y @inputs -filter_complex "vstack=inputs=$($rows.Count)" "$out\transitions.png"
Write-Host "previews in $out"

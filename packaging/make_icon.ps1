param([string]$src, [string]$out)
Add-Type -AssemblyName System.Drawing

# 按内容包围盒裁剪四周空白：扫描非白/非透明像素，居中裁成正方形，保留少量边距
function Trim-Blank($img) {
    $w = $img.Width; $h = $img.Height
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::White)
    $g.DrawImage($img, 0, 0, $w, $h)
    $g.Dispose()
    $minX = $w; $minY = $h; $maxX = -1; $maxY = -1
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $c = $bmp.GetPixel($x, $y)
            if ($c.A -lt 240 -or [Math]::Abs($c.R - 255) -gt 24 -or [Math]::Abs($c.G - 255) -gt 24 -or [Math]::Abs($c.B - 255) -gt 24) {
                if ($x -lt $minX) { $minX = $x }
                if ($x -gt $maxX) { $maxX = $x }
                if ($y -lt $minY) { $minY = $y }
                if ($y -gt $maxY) { $maxY = $y }
            }
        }
    }
    $bmp.Dispose()
    if ($maxX -lt 0) {
        return $img
    }   # 全空白，不裁剪
    $pad = [Math]::Round([Math]::Max($w, $h) * 0.02)
    $minX = [Math]::Max(0, $minX - $pad); $minY = [Math]::Max(0, $minY - $pad)
    $maxX = [Math]::Min($w - 1, $maxX + $pad); $maxY = [Math]::Min($h - 1, $maxY + $pad)
    $cw = $maxX - $minX + 1; $ch = $maxY - $minY + 1
    $side = [Math]::Max($cw, $ch)
    $cx = [Math]::Round(($minX + $maxX) / 2); $cy = [Math]::Round(($minY + $maxY) / 2)
    $sx0 = [Math]::Max(0, $cx - [Math]::Floor($side / 2)); $sy0 = [Math]::Max(0, $cy - [Math]::Floor($side / 2))
    $sx0 = [Math]::Min($sx0, $w - $side); $sy0 = [Math]::Min($sy0, $h - $side)
    if ($sx0 -lt 0) { $sx0 = 0 }; if ($sy0 -lt 0) { $sy0 = 0 }
    $crop = New-Object System.Drawing.Bitmap($side, $side)
    $g2 = [System.Drawing.Graphics]::FromImage($crop)
    $g2.InterpolationMode = 'HighQualityBicubic'
    $g2.Clear([System.Drawing.Color]::Transparent)
    $g2.DrawImage($img, [System.Drawing.Rectangle]::new(0, 0, $side, $side), [System.Drawing.Rectangle]::new($sx0, $sy0, $side, $side), [System.Drawing.GraphicsUnit]::Pixel)
    $g2.Dispose()
    return $crop
}

$img = [System.Drawing.Image]::FromFile($src)
$trimmed = Trim-Blank $img
$sizes = @(16, 32, 48, 64, 128, 256)
$pngs = @{}
foreach ($s in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap($s, $s)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = 'HighQualityBicubic'
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($trimmed, 0, 0, $s, $s)
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs[$s] = $ms.ToArray()
    $bmp.Dispose()
}
$img.Dispose(); $trimmed.Dispose()
$fs = [System.IO.File]::Create($out)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $w = 0; if ($s -lt 256) { $w = $s }
    $bw.Write([Byte]$w); $bw.Write([Byte]$w)
    $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$pngs[$s].Length); $bw.Write([UInt32]$offset)
    $offset += $pngs[$s].Length
}
foreach ($s in $sizes) { $bw.Write($pngs[$s]) }
$bw.Close(); $fs.Close()
Write-Output ("ico_bytes=" + (Get-Item $out).Length)

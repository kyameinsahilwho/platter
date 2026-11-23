# Draws the NSIS installer bitmaps (24-bit BMP, the sizes MUI2 expects) in Platter's palette.
#   powershell -File installer\make-art.ps1
Add-Type -AssemblyName System.Drawing
$here = $PSScriptRoot
$pine = [Drawing.Color]::FromArgb(20, 98, 79)
$pineDeep = [Drawing.Color]::FromArgb(13, 72, 58)
$paper = [Drawing.Color]::FromArgb(237, 239, 234)
$surface = [Drawing.Color]::FromArgb(250, 251, 248)
$saffron = [Drawing.Color]::FromArgb(232, 181, 74)
$ink = [Drawing.Color]::FromArgb(21, 32, 27)
$cellFree = [Drawing.Color]::FromArgb(36, 116, 95)
$cellLow = [Drawing.Color]::FromArgb(64, 140, 118)
$cellMid = [Drawing.Color]::FromArgb(140, 190, 172)

function New-Font([float]$size, [Drawing.FontStyle]$style) {
  foreach ($f in "Segoe UI Variable Display", "Segoe UI Semibold", "Segoe UI") {
    try { $font = New-Object Drawing.Font($f, $size, $style, [Drawing.GraphicsUnit]::Pixel); if ($font.Name -eq $f) { return $font } } catch {}
  }
  New-Object Drawing.Font("Arial", $size, $style, [Drawing.GraphicsUnit]::Pixel)
}

# The Platter mark: paper disc, saffron sector, two track rings, pine hub.
function Draw-Mark($g, [float]$cx, [float]$cy, [float]$r, $ringColor) {
  $g.FillEllipse((New-Object Drawing.SolidBrush $paper), $cx - $r, $cy - $r, 2 * $r, 2 * $r)
  $g.FillPie((New-Object Drawing.SolidBrush $saffron), $cx - $r, $cy - $r, 2 * $r, 2 * $r, -90, 62)
  $pen = New-Object Drawing.Pen($ringColor, [Math]::Max(1.2, $r * 0.035))
  foreach ($k in 0.74, 0.97) { $rr = $r * $k * 0.5 + $r * 0.25; $g.DrawEllipse($pen, $cx - $rr, $cy - $rr, 2 * $rr, 2 * $rr) }
  $h = $r * 0.3
  $g.FillEllipse((New-Object Drawing.SolidBrush $ringColor), $cx - $h, $cy - $h, 2 * $h, 2 * $h)
  $d = $r * 0.1
  $g.FillEllipse((New-Object Drawing.SolidBrush $paper), $cx - $d, $cy - $d, 2 * $d, 2 * $d)
}

# --- Welcome / finish sidebar: 164 x 314 ---
$w = 164; $h = 314
$bmp = New-Object Drawing.Bitmap($w, $h, [Drawing.Imaging.PixelFormat]::Format24bppRgb)
$g = [Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = "AntiAlias"; $g.TextRenderingHint = "AntiAliasGridFit"
$g.Clear($pine)
# Allocation-map strip along the bottom: the app's signature, in physical order, filling up.
$cols = 12; $rows = 6; $pitch = 12; $cell = 9
$x0 = ($w - ($cols * $pitch - ($pitch - $cell))) / 2; $y0 = $h - 20 - $rows * $pitch
$rand = New-Object Random 7
for ($i = 0; $i -lt $cols * $rows; $i++) {
  $c = $i % $cols; $r = [Math]::Floor($i / $cols)
  $fill = 1.0 - ($i / ($cols * $rows)) + ($rand.NextDouble() - 0.5) * 0.5
  $color = if ($fill -gt 0.75) { $paper } elseif ($fill -gt 0.45) { $cellMid } elseif ($fill -gt 0.2) { $cellLow } else { $cellFree }
  $g.FillRectangle((New-Object Drawing.SolidBrush $color), $x0 + $c * $pitch, $y0 + $r * $pitch, $cell, $cell)
}
Draw-Mark $g ($w / 2) 72 38 $pine
$fmt = New-Object Drawing.StringFormat; $fmt.Alignment = "Center"
$g.DrawString("Platter", (New-Font 26 ([Drawing.FontStyle]::Bold)), (New-Object Drawing.SolidBrush $paper), (New-Object Drawing.RectangleF(0, 118, $w, 36)), $fmt)
$sub = [Drawing.Color]::FromArgb(205, 226, 218)
$g.DrawString("See, clean and tune`nthe drives on this PC", (New-Font 12.5 ([Drawing.FontStyle]::Regular)), (New-Object Drawing.SolidBrush $sub), (New-Object Drawing.RectangleF(8, 154, ($w - 16), 40)), $fmt)
$bmp.Save((Join-Path $here "sidebar.bmp"), [Drawing.Imaging.ImageFormat]::Bmp)
$g.Dispose(); $bmp.Dispose()

# --- Page header: 150 x 57 (sits at the right of a white header band) ---
$w = 150; $h = 57
$bmp = New-Object Drawing.Bitmap($w, $h, [Drawing.Imaging.PixelFormat]::Format24bppRgb)
$g = [Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = "AntiAlias"; $g.TextRenderingHint = "AntiAliasGridFit"
$g.Clear([Drawing.Color]::White)
$tile = New-Object Drawing.Drawing2D.GraphicsPath; $rr = 10
foreach ($a in @(@(98, 8, 180), @(129, 8, 270), @(129, 39, 0), @(98, 39, 90))) { $tile.AddArc($a[0], $a[1], $rr, $rr, $a[2], 90) }
$tile.CloseFigure(); $g.FillPath((New-Object Drawing.SolidBrush $pine), $tile)
Draw-Mark $g 118.5 28.5 15.5 $pine
$fmtR = New-Object Drawing.StringFormat; $fmtR.Alignment = "Far"; $fmtR.LineAlignment = "Center"
$g.DrawString("Platter", (New-Font 17 ([Drawing.FontStyle]::Bold)), (New-Object Drawing.SolidBrush $ink), (New-Object Drawing.RectangleF(0, 8, 91, 41)), $fmtR)
$bmp.Save((Join-Path $here "header.bmp"), [Drawing.Imaging.ImageFormat]::Bmp)
$g.Dispose(); $bmp.Dispose()
"art written to $here"

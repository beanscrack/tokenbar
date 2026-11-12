Add-Type -AssemblyName System.Drawing
# Generates the Discord App Icon + Rich Presence art for TokenBar.
# Output: %USERPROFILE%\Downloads\tokenbar_assets\{app_icon.png (512), tokenbar.png (128)}
$out = Join-Path ([Environment]::GetFolderPath('Downloads')) 'tokenbar_assets'
New-Item -ItemType Directory -Force -Path $out | Out-Null

function New-Icon([int]$size, [string]$path) {
  $bmp = New-Object System.Drawing.Bitmap($size, $size)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'
  $g.TextRenderingHint = 'AntiAliasGridFit'
  $r = [int]($size * 0.22)
  $bg = New-Object System.Drawing.Drawing2D.GraphicsPath
  $d = $r * 2
  $bg.AddArc(0, 0, $d, $d, 180, 90)
  $bg.AddArc($size - $d - 1, 0, $d, $d, 270, 90)
  $bg.AddArc($size - $d - 1, $size - $d - 1, $d, $d, 0, 90)
  $bg.AddArc(0, $size - $d - 1, $d, $d, 90, 90)
  $bg.CloseFigure()
  $c1 = [System.Drawing.Color]::FromArgb(255, 17, 19, 18)
  $c2 = [System.Drawing.Color]::FromArgb(255, 23, 26, 25)
  $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
  $br = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $c1, $c2, 90)
  $g.FillPath($br, $bg)
  $colw = [int]($size * 0.62)
  $colh = [int]($size * 0.13)
  $x0 = [int](($size - $colw) / 2)
  $y0 = [int]($size * 0.43)
  $gap = [int]($size * 0.015)
  $cols = @(
    @(0.55, [System.Drawing.Color]::FromArgb(255, 52, 211, 153)),
    @(0.22, [System.Drawing.Color]::FromArgb(255,  45, 212, 191)),
    @(0.09, [System.Drawing.Color]::FromArgb(255,  96, 165, 250)),
    @(0.14, [System.Drawing.Color]::FromArgb(255, 251, 146,  60))
  )
  $x = $x0
  foreach ($c in $cols) {
    $w = [int]($colw * $c[0]) - $gap
    if ($w -lt 2) { $w = 2 }
    $gp = New-Object System.Drawing.Drawing2D.GraphicsPath
    $dd = $colh
    $gp.AddArc($x, $y0, $dd, $dd, 90, 180)
    $gp.AddArc($x + $w - $dd, $y0, $dd, $dd, 270, 180)
    $gp.CloseFigure()
    $b = New-Object System.Drawing.SolidBrush($c[1])
    $g.FillPath($b, $gp)
    $x += $w + $gap
  }
  $fs = [float]($size * 0.16)
  $fnt = New-Object System.Drawing.Font('Segoe UI', $fs, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
  $tb = New-Object System.Drawing.StringFormat
  $tb.Alignment = 'Center'
  $tb.LineAlignment = 'Center'
  $rect2 = New-Object System.Drawing.RectangleF(0, ($size * 0.56), $size, ($size * 0.34))
  $g.DrawString('TokenBar', $fnt, [System.Drawing.Brushes]::White, $rect2, $tb)
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  Write-Output "wrote $path"
}

New-Icon 512 (Join-Path $out 'tokenbar_512.png')
New-Icon 128 (Join-Path $out 'tokenbar_128.png')
Copy-Item (Join-Path $out 'tokenbar_512.png') (Join-Path $out 'app_icon.png') -Force
Copy-Item (Join-Path $out 'tokenbar_128.png') (Join-Path $out 'tokenbar.png') -Force
Write-Output "READY: $out"

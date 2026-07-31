Add-Type -AssemblyName System.Drawing

$outputDirectory = Join-Path $PSScriptRoot '..\assets'
$outputPath = Join-Path $outputDirectory 'OpenPaper.ico'
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null

$bitmap = [System.Drawing.Bitmap]::new(256, 256, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$graphics.Clear([System.Drawing.Color]::Black)

function Add-PanelPath([System.Drawing.Drawing2D.GraphicsPath] $path, [double[]] $points) {
    $path.StartFigure()
    $path.AddLine($points[0], $points[1], $points[2], $points[3])
    $path.AddBezier($points[2], $points[3], $points[4], $points[5], $points[6], $points[7], $points[8], $points[9])
    $path.AddLine($points[8], $points[9], $points[10], $points[11])
    $path.AddBezier($points[10], $points[11], $points[12], $points[13], $points[14], $points[15], $points[16], $points[17])
    $path.CloseFigure()
}

# The geometry mirrors gemini-svg.svg, scaled from 500x500 to 256x256.
$scale = 0.512
$translate = 128
$panels = @(
    @{ Opacity = 242; Points = @(-20,-140,90,-110,140,-60,150,20,110,80,10,-10,50,-50,20,-100,-20,-140) },
    @{ Opacity = 166; Points = @(20,140,-90,110,-140,60,-150,-20,-110,-80,-10,10,-50,50,-20,100,20,140) },
    @{ Opacity = 89; Points = @(-110,-40,-30,-120,40,-150,120,-80,80,40,-20,-20,20,-50,-40,-70,-110,-40) }
)

foreach ($panel in $panels) {
    $path = [System.Drawing.Drawing2D.GraphicsPath]::new()
    $scaled = foreach ($value in $panel.Points) { [single]($value * $scale + $translate) }
    Add-PanelPath $path $scaled
    $brush = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb($panel.Opacity, 255, 255, 255))
    $graphics.FillPath($brush, $path)
    $brush.Dispose(); $path.Dispose()
}

$icon = [System.Drawing.Icon]::FromHandle($bitmap.GetHicon())
$stream = [System.IO.File]::Create($outputPath)
$icon.Save($stream)
$stream.Dispose(); $icon.Dispose(); $graphics.Dispose(); $bitmap.Dispose()

Write-Output "Created $outputPath"

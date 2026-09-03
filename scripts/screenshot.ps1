# Capture the running sc-win window to a PNG.
#
#     powershell -ExecutionPolicy Bypass -File scripts/screenshot.ps1 -Out shot.png
#
# Uses PrintWindow with PW_RENDERFULLCONTENT rather than a screen grab: the window
# is GPU-composited (wgpu/iced), and CopyFromScreen returns whatever pixels sit at
# those coordinates -- the DESKTOP WALLPAPER if the window never came to the front,
# which is exactly what the first attempt produced. PrintWindow asks the window to
# render itself, so it works behind other windows.
#
# Written because "does this look right?" was being answered by reading code and
# waiting for the user to send a picture.

param([string]$Out = "shot.png")

Add-Type -AssemblyName System.Drawing

# PrintWindow asks the window to render ITSELF into a bitmap, so it works even when
# the window is behind others or off-screen -- unlike CopyFromScreen, which grabs
# whatever pixels happen to be at those coordinates (the wallpaper, if the window
# never came to the front).
$sig = @'
using System;
using System.Runtime.InteropServices;
public class Win2 {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [StructLayout(LayoutKind.Sequential)]
  public struct RECT { public int Left, Top, Right, Bottom; }
}
'@
Add-Type -TypeDefinition $sig

$p = Get-Process sc-win -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output "NOT RUNNING"; exit 1 }
$h = $p.MainWindowHandle
if ($h -eq 0) { Write-Output "NO WINDOW HANDLE"; exit 1 }

# A minimised window has nothing to render; restore first.
if ([Win2]::IsIconic($h)) { [void][Win2]::ShowWindow($h, 9); Start-Sleep -Milliseconds 900 }
[void][Win2]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 600

$r = New-Object Win2+RECT
[void][Win2]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left
$hgt = $r.Bottom - $r.Top
if ($w -le 0 -or $hgt -le 0) { Write-Output "BAD RECT"; exit 1 }

$bmp = New-Object System.Drawing.Bitmap $w, $hgt
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
# flag 2 = PW_RENDERFULLCONTENT, needed for GPU-composited windows like wgpu/iced.
$ok = [Win2]::PrintWindow($h, $dc, 2)
$g.ReleaseHdc($dc)
$g.Dispose()

if (-not $ok) {
  # Fall back to a screen grab now that the window has been raised.
  $bmp.Dispose()
  $bmp = New-Object System.Drawing.Bitmap $w, $hgt
  $g2 = [System.Drawing.Graphics]::FromImage($bmp)
  $g2.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
  $g2.Dispose()
  Write-Output "PrintWindow failed; used screen grab"
}

$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "SAVED $Out ($w x $hgt)"

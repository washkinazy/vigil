#!/usr/bin/env python3
"""Regenerate embedded ARGB tray icons from SVG (developer tools: Inkscape, ImageMagick)."""
from pathlib import Path
import os
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1] / "assets/tray"
with tempfile.TemporaryDirectory(prefix="vigil-icons-") as directory:
    for name in ("enabled", "disabled", "unavailable"):
        for size in (16, 22, 24, 32, 48):
            png = Path(directory) / "icon.png"
            subprocess.run([
                "inkscape", str(root / f"{name}.svg"), "--export-type=png",
                f"--export-width={size}", f"--export-height={size}",
                f"--export-filename={png}",
            ], check=True, stdout=subprocess.DEVNULL, env={**os.environ, "INKSCAPE_PROFILE_DIR": directory})
            rgba = subprocess.check_output(["magick", str(png), "-depth", "8", "rgba:-"])
            assert len(rgba) == size * size * 4
            argb = bytearray()
            for offset in range(0, len(rgba), 4):
                argb.extend(rgba[offset + 3:offset + 4] + rgba[offset:offset + 3])
            (root / f"{name}-{size}.argb").write_bytes(argb)

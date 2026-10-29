#!/usr/bin/env python3
"""Makes the static font files Lazy bundles from the variable fonts of the brand kit.

Why static: the text engine on Linux (cosmic-text) cannot choose a weight or optical size from a
variable font's axes, so a variable file would always draw its default instance (DM Sans at its
9 point optical size, Nunito at its lightest weight, 200). A static instance has one weight.

    python3 -m venv /tmp/ft && /tmp/ft/bin/pip install fonttools
    /tmp/ft/bin/python crates/lazy-ui/assets/fonts/make-static-fonts.py \
        branding/fonts/dmsans/DMSans-Variable.ttf  DMSans-Italic[opsz,wght].ttf \
        branding/fonts/nunito/Nunito-Variable.ttf

DMSans-Italic[opsz,wght].ttf is the upright's companion in https://github.com/google/fonts
(ofl/dmsans); the brand kit supplies only the upright. Both families are SIL OFL 1.1.
"""
import sys
from pathlib import Path

from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

HERE = Path(__file__).resolve().parent


def make(src, axes, out, *, family, style, ps, weight, italic=False, bold=False, typo_family=None, typo_style=None):
    font = TTFont(src)
    font = instantiateVariableFont(font, axes, inplace=False)
    name = font["name"]
    full = f"{family} {style}" if style not in ("Regular",) else family
    # Drop the variable font's name records that no longer apply, then write the static ones.
    for name_id in (1, 2, 3, 4, 6, 16, 17, 21, 22, 25):
        name.removeNames(nameID=name_id)
    for plat, enc, lang in ((3, 1, 0x409), (1, 0, 0)):
        name.setName(family, 1, plat, enc, lang)
        name.setName("Italic" if italic and not bold and weight == 400 else ("Bold" if bold else "Regular") if not italic else ("Bold Italic" if bold else "Italic"), 2, plat, enc, lang)
        name.setName(f"{ps};static", 3, plat, enc, lang)
        name.setName(f"{typo_family or family} {typo_style or style}".strip(), 4, plat, enc, lang)
        name.setName(ps, 6, plat, enc, lang)
        if typo_family:
            name.setName(typo_family, 16, plat, enc, lang)
            name.setName(typo_style, 17, plat, enc, lang)
    os2 = font["OS/2"]
    os2.usWeightClass = weight
    sel = os2.fsSelection & ~(0b1 | 0b100000 | 0b1000000)   # clear italic, bold, regular
    sel |= 0b1 if italic else 0
    sel |= 0b100000 if bold else 0
    sel |= 0b1000000 if not italic and not bold else 0
    os2.fsSelection = sel
    font["head"].macStyle = (0b1 if bold else 0) | (0b10 if italic else 0)
    if "STAT" in font:
        del font["STAT"]
    font.save(out)
    print("wrote", out)


def main(argv):
    dm, dm_italic, nunito = argv[1:4]
    dmout = HERE / "dm-sans"
    # DM Sans: optical size 14 suits the interface's 13 to 15 pixel text.
    opsz = {"opsz": 14}
    make(dm, {**opsz, "wght": 400}, dmout / "DMSans-Regular.ttf", family="DM Sans", style="Regular", ps="DMSans-Regular", weight=400)
    make(dm_italic, {**opsz, "wght": 400}, dmout / "DMSans-Italic.ttf", family="DM Sans", style="Italic", ps="DMSans-Italic", weight=400, italic=True)
    make(dm, {**opsz, "wght": 600}, dmout / "DMSans-SemiBold.ttf", family="DM Sans SemiBold", style="Regular", ps="DMSans-SemiBold", weight=600, typo_family="DM Sans", typo_style="SemiBold")
    make(dm_italic, {**opsz, "wght": 600}, dmout / "DMSans-SemiBoldItalic.ttf", family="DM Sans SemiBold", style="Italic", ps="DMSans-SemiBoldItalic", weight=600, italic=True, typo_family="DM Sans", typo_style="SemiBold Italic")
    # Nunito: Bold, for the few headings (onboarding and dialogs).
    make(nunito, {"wght": 700}, HERE / "nunito" / "Nunito-Bold.ttf", family="Nunito", style="Bold", ps="Nunito-Bold", weight=700, bold=True)


if __name__ == "__main__":
    main(sys.argv)

# dmgbuild settings for tools/package.sh: the classic window, hyperMAC.app on the left, an arrow
# (in assets/dmg-background.tiff, tools/mkdmgbg.swift) to the Applications link on the right,
# README and the third-party notices in a row below. dmgbuild writes the .DS_Store itself, so no
# Finder scripting (no Automation prompt). package.sh passes -D stage=<folder with the files>.
import os

stage = defines["stage"]  # noqa: F821 (dmgbuild provides `defines`)

format = "UDZO"
filesystem = "HFS+"
# (the third-party notices in one folder: five long file names in a row ran into each other)
files = [os.path.join(stage, n) for n in ("hyperMAC.app", "README.txt", "Licenses")]
symlinks = {"Applications": "/Applications"}
icon_locations = {
    "hyperMAC.app": (170, 190),
    "Applications": (490, 190),
    "README.txt": (250, 420),
    "Licenses": (410, 420),
}
background = defines["background"]  # noqa: F821
window_rect = ((200, 120), (660, 548))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
icon_size = 96
text_size = 12
arrange_by = None

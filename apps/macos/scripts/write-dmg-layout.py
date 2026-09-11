#!/usr/bin/env python3
"""マウント済みの配布DMGに、宇宙→地球のFinder配置を保存する。

ビルド時のみds-store==1.3.3、mac-alias==2.2.3を使う。
Finderの操作、利用者の表示設定変更、マウント・アンマウントは行わない。
"""

import argparse
import os
from pathlib import Path

from ds_store import DSStore
from mac_alias import Alias


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mount", type=Path)
    args = parser.parse_args()
    mount = args.mount
    if not mount.is_absolute() or not os.path.ismount(mount):
        parser.error("配布用DMGのマウント先を絶対パスで指定してください。")
    if not (mount / "Polaris.app/Contents/Info.plist").is_file():
        parser.error("配布用Polaris.appがありません。")
    applications = mount / "Applications"
    if not applications.is_symlink() or applications.readlink() != Path("/Applications"):
        parser.error("Applicationsは/Applicationsへのリンクにしてください。")
    background = mount / ".background/space-earth.png"
    if not background.is_file() or background.is_symlink():
        parser.error("配布用の背景画像がありません。")
    with DSStore.open(str(mount / ".DS_Store"), "w+") as store:
        store["."]["bwsp"] = {
            "ShowStatusBar": False, "ShowTabView": False, "ShowToolbar": False,
            "ShowPathbar": False, "ShowSidebar": False, "ContainerShowSidebar": False,
            "WindowBounds": "{{160, 120}, {640, 548}}", "PreviewPaneVisibility": False,
        }
        store["."]["icvp"] = {
            "viewOptionsVersion": 1, "backgroundType": 2,
            "backgroundImageAlias": Alias.for_file(str(background)).to_bytes(),
            "backgroundColorRed": 0.035, "backgroundColorGreen": 0.065,
            "backgroundColorBlue": 0.10, "gridOffsetX": 0.0, "gridOffsetY": 0.0,
            "gridSpacing": 100.0, "arrangeBy": "none", "showIconPreview": True,
            "showItemInfo": False, "labelOnBottom": True, "textSize": 14.0,
            "iconSize": 96.0, "scrollPositionX": 0.0, "scrollPositionY": 0.0,
        }
        store["."]["vSrn"] = ("long", 1)
        store["."]["icvl"] = ("type", b"icnv")
        store["Polaris.app"]["Iloc"] = (320, 112)
        store["Applications"]["Iloc"] = (320, 380)
    with DSStore.open(str(mount / ".DS_Store"), "r") as store:
        assert store["Polaris.app"]["Iloc"] == (320, 112)
        assert store["Applications"]["Iloc"] == (320, 380)
        assert store["."]["icvp"]["backgroundType"] == 2
    print("DMG内の上下配置と背景参照を保存・照合しました。")


if __name__ == "__main__":
    main()

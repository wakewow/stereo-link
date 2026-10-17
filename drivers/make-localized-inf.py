#!/usr/bin/env python3
"""Generate the miplay-bridge localized Scream INF from the pristine upstream INF.

Why a generator instead of hand-editing:
  * the INF must be UTF-16LE + BOM because it now contains a non-ASCII device
    name ("xiaomi" device name in Chinese) - INF files with non-ASCII strings are
    required to be Unicode;
  * keeping the upstream file untouched means the exact local change stays
    reviewable (the script is the diff).

Inputs / outputs (all inside drivers/scream-derived/):
  Scream.upstream.inf  pristine copy of upstream Scream.inf (never modified)
  Scream.inf           generated, localized - this is what sign.ps1 stages

Usage:
    python make-localized-inf.py            # generate Scream.inf + print the diff
    python make-localized-inf.py --verify   # only check Scream.upstream.inf hash + shelf-check
"""
import hashlib
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
SRC_DIR = HERE / "scream-derived"
UPSTREAM = SRC_DIR / "Scream.upstream.inf"
TARGET = SRC_DIR / "Scream.inf"

# ---------------------------------------------------------------- local changes
DEVICE_NAME = "小米妙播设备"          # -> "扬声器 (小米妙播设备)" in the Sound app
SERVICE_NAME = "小米妙播设备 (虚拟声卡)"

REPLACEMENTS = [
    ('Scream.DeviceDesc="Scream (WDM)"', f'Scream.DeviceDesc="{DEVICE_NAME}"'),
    ('Scream.SvcDesc="Scream (WDM)"', f'Scream.SvcDesc="{SERVICE_NAME}"'),
    ('Scream.Name="Scream"', f'Scream.Name="{DEVICE_NAME}"'),
    ('Scream.Wave.szPname="Scream Wave"', f'Scream.Wave.szPname="{DEVICE_NAME}"'),
    ('Scream.Topo.szPname="Scream Topology"', f'Scream.Topo.szPname="{DEVICE_NAME}"'),
]

# FriendlyName written straight into the device hardware key (Enum\<...>\<n>) as a
# belt-and-braces companion to %.DeviceDesc%. HKR inside a DDInstall.HW section is
# the device's hardware key, whereas HKR inside DDInstall.AddReg is the driver's
# software key - that is why a separate .HW section is required.
HW_SECTION = """[Scream.NT.HW]
AddReg=Scream.NT.HW.AddReg

[Scream.NT.HW.AddReg]
HKR,,FriendlyName,,%Scream.DeviceDesc%

"""

HEADER = """; ============================================================================
; Scream.inf - miplay-bridge localized variant (GENERATED, do not hand-edit)
;
; Upstream : https://github.com/duncanthrax/scream  commit d789743c248b11d1df7e5ecc546b1bc60b90cd91
; License  : MS-PL (see LICENSE-MS-PL.txt in this directory)
; Generator: drivers/make-localized-inf.py
;
; Differences from upstream Scream.inf (the only ones):
;   1. device/service/media-category display names are Chinese so the audio
;      endpoint shows up as "扬声器 (小米妙播设备)" instead of "Speakers (Scream (WDM))";
;   2. added [Scream.NT.HW] / [Scream.NT.HW.AddReg] which writes FriendlyName into
;      the device hardware key as well;
;   3. this comment block.
; Nothing else - binary, service key name ("Scream"), hardware id (*Scream),
; ClassGUID, catalog name and DriverVer are unchanged.
; ============================================================================
"""


def upstream_bytes() -> bytes:
    if UPSTREAM.exists():
        return UPSTREAM.read_bytes()
    # first run: snapshot the pristine file that sign.ps1 staged (scream-derived/Scream.inf)
    raw = TARGET.read_bytes()
    UPSTREAM.write_bytes(raw)
    return raw


def decode(raw: bytes) -> str:
    if raw.startswith(b"\xff\xfe"):
        return raw.decode("utf-16-le").lstrip("\ufeff")
    if raw.startswith(b"\xef\xbb\xbf"):
        return raw.decode("utf-8-sig")
    return raw.decode("utf-8")


def main() -> int:
    raw = upstream_bytes()
    text = decode(raw)
    print(f"upstream : {len(raw)} bytes  sha256={hashlib.sha256(raw).hexdigest()}")
    print(f"           crlf={b'\\r\\n' in raw}  bom={raw[:2].hex()}")

    if "--verify" in sys.argv:
        return 0

    for old, new in REPLACEMENTS:
        if text.count(old) != 1:
            print(f"ERROR: expected exactly one occurrence of {old!r}, found {text.count(old)}")
            return 2
        text = text.replace(old, new)

    anchor = "[Scream.NT]\n"
    if anchor not in text:
        anchor = "[Scream.NT]\r\n"
    if text.count(anchor) != 1:
        print("ERROR: cannot find a single [Scream.NT] anchor")
        return 2
    text = text.replace(anchor, HW_SECTION + anchor)

    text = HEADER + text
    text = text.replace("\r\n", "\n").replace("\n", "\r\n")   # INF files are CRLF

    out = b"\xff\xfe" + text.encode("utf-16-le")               # UTF-16LE + BOM
    TARGET.write_bytes(out)
    print(f"generated: {TARGET}  {len(out)} bytes  UTF-16LE+BOM  crlf=True")

    print("\n--- decoded local changes ---")
    for line in text.split("\r\n"):
        s = line.strip()
        if ("szPname" in s or "DeviceDesc" in s or "SvcDesc" in s or s.startswith("Scream.Name")
                or "FriendlyName" in s or s.startswith("[Scream.NT.HW")):
            print("   " + s)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

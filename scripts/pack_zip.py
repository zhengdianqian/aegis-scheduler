import os
import sys
import time
import zipfile

# Reproducible module archive layout:
#   create_system = 0 (MS-DOS/FAT), every file 100644, every directory 40755,
#   explicit directory entries, deflated files, stored empty dirs.
# KernelSU / Magisk own installers set executable bits at install time, so the
# archive itself never needs 0755.
stage, out = sys.argv[1], sys.argv[2]


def info_for(arc, is_dir):
    info = zipfile.ZipInfo(arc + ("/" if is_dir else ""), date_time=time.localtime()[:6])
    info.create_system = 0
    info.external_attr = (0o40755 if is_dir else 0o100644) << 16
    info.compress_type = zipfile.ZIP_STORED if is_dir else zipfile.ZIP_DEFLATED
    return info


with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for root, dirs, files in os.walk(stage):
        dirs.sort()
        files.sort()
        for name in dirs:
            arc = os.path.relpath(os.path.join(root, name), stage).replace("\\", "/")
            zf.writestr(info_for(arc, True), b"")
        for name in files:
            full = os.path.join(root, name)
            arc = os.path.relpath(full, stage).replace("\\", "/")
            with open(full, "rb") as fh:
                zf.writestr(info_for(arc, False), fh.read())
print(f"packed {out}")

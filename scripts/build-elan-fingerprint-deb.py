#!/usr/bin/env python3
"""Package the owner's working Lenovo ELAN 0.0.8 OpenSSL-3 TOD driver, without installation.

The vendor binary stays outside the repository; no fingerprint templates or captures are copied.
"""
import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
DRIVER_SHA256 = "a5060ccdfc0c613bd4522be29f87cd94f098e6b83294cf02cd5a958b24b27f80"
VERSION = "0.0.8+templaros1"
CONTROL = f"""Package: templaros-elan-fingerprint
Version: {VERSION}
Section: admin
Priority: optional
Architecture: amd64
Maintainer: TemplarOS Developers
Depends: libfprint-2-tod1 (>= 1:1.94.9), libfprint-2-2, libssl3t64, fprintd, libpam-fprintd
Conflicts: libfprint-2-tod1-elan
Description: Owner-supplied Lenovo ELAN MOH fingerprint TOD integration
 Packages the verified OpenSSL-3 Lenovo 0.0.8 plugin for the reference
 04f3:0c4b revision 24.30 sensor, plus its scoped autosuspend workaround.
 Enrollment remains managed by fprintd; no private biometric data is shipped.
"""
RULE = """# ELAN 04f3:0c4b revision 24.30: keep the owner's working area/MOH sensor powered.
# Other modules sharing this product ID are not affected.
SUBSYSTEM=="usb", ATTR{idVendor}=="04f3", ATTR{idProduct}=="0c4b", ATTR{bcdDevice}=="2430", TEST=="power/control", ATTR{power/control}="on"
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("driver", type=Path, help="working OpenSSL-3 libfprint-2-tod1-elan.so")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "target/debian")
    args = parser.parse_args()
    data = args.driver.read_bytes()
    if hashlib.sha256(data).hexdigest() != DRIVER_SHA256:
        parser.error("driver checksum differs from the working Lubuntu Lenovo OpenSSL-3 plugin; do not substitute the older OpenSSL-1.1 ELAN blob")
    dynamic = subprocess.check_output(["readelf", "-d", str(args.driver)], text=True)
    if "libcrypto.so.3" not in dynamic or "libcrypto.so.1.1" in dynamic:
        parser.error("driver requires the wrong OpenSSL ABI")
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    package = output / f"templaros-elan-fingerprint_{VERSION}_amd64.deb"
    with tempfile.TemporaryDirectory(prefix="templaros-elan-") as temporary:
        stage = Path(temporary)
        for relative, content in {
            "DEBIAN/control": CONTROL,
            "usr/lib/udev/rules.d/92-templaros-elan-fingerprint-nosuspend.rules": RULE,
        }.items():
            path = stage / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
            path.chmod(0o644)
        driver = stage / "usr/lib/x86_64-linux-gnu/libfprint-2/tod-1/libfprint-2-tod1-elan.so"
        driver.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(args.driver, driver)
        driver.chmod(0o644)
        subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(stage), str(package)], check=True)
    print(package)
    print(f"Driver SHA-256: {DRIVER_SHA256}; package only, nothing installed or restarted.")


if __name__ == "__main__":
    main()

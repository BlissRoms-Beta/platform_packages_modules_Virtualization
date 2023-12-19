"""Extracts the following hashes from the AVB footer of Microdroid's kernel:

- kernel hash
- initrd_normal hash
- initrd_debug hash

The hashes are written to stdout as a Rust file.

In unsupportive environments such as x86, when the kernel is just an empty file,
the output Rust file has the same hash constant fields for compatibility
reasons, but all of them are empty.
"""
#!/usr/bin/env python3

import sys
import subprocess
from typing import Dict

PARTITION_NAME_BOOT = 'boot'
PARTITION_NAME_INITRD_NORMAL = 'initrd_normal'
PARTITION_NAME_INITRD_DEBUG = 'initrd_debug'

def main(args):
    """Main function."""
    avbtool = args[0]
    kernel_image_path = args[1]
    hashes = collect_hashes(avbtool, kernel_image_path)

    print("//! This file is generated by extract_microdroid_kernel_hashes.py.")
    print("//! It contains the hashes of the kernel and initrds.\n")
    print("#![no_std]\n#![allow(missing_docs)]\n")

    # Microdroid's kernel is just an empty file in unsupportive environments
    # such as x86, in this case the hashes should be empty.
    if hashes.keys() != {PARTITION_NAME_BOOT,
                         PARTITION_NAME_INITRD_NORMAL,
                         PARTITION_NAME_INITRD_DEBUG}:
        print("/// The kernel is empty, no hashes are available.")
        hashes[PARTITION_NAME_BOOT] = ""
        hashes[PARTITION_NAME_INITRD_NORMAL] = ""
        hashes[PARTITION_NAME_INITRD_DEBUG] = ""

    print("pub const KERNEL_HASH: &[u8] = &["
          f"{format_hex_string(hashes[PARTITION_NAME_BOOT])}];\n")
    print("pub const INITRD_NORMAL_HASH: &[u8] = &["
          f"{format_hex_string(hashes[PARTITION_NAME_INITRD_NORMAL])}];\n")
    print("pub const INITRD_DEBUG_HASH: &[u8] = &["
          f"{format_hex_string(hashes[PARTITION_NAME_INITRD_DEBUG])}];")

def collect_hashes(avbtool: str, kernel_image_path: str) -> Dict[str, str]:
    """Collects the hashes from the AVB footer of the kernel image."""
    hashes = {}
    with subprocess.Popen(
        [avbtool, 'print_partition_digests', '--image', kernel_image_path],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as proc:
        stdout, _ = proc.communicate()
        for line in stdout.decode("utf-8").split("\n"):
            line = line.replace(" ", "").split(":")
            if len(line) == 2:
                partition_name, hash_ = line
                hashes[partition_name] = hash_
    return hashes

def format_hex_string(hex_string: str) -> str:
    """Formats a hex string into a Rust array."""
    assert len(hex_string) % 2 == 0, \
          "Hex string must have even length: " + hex_string
    return ", ".join(["\n0x" + hex_string[i:i+2] if i % 32 == 0
                       else "0x" + hex_string[i:i+2]
                       for i in range(0, len(hex_string), 2)])

if __name__ == '__main__':
    main(sys.argv[1:])
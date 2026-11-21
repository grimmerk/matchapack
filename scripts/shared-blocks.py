#!/usr/bin/env python3
"""How much of a tree's node_modules shares its blocks with a store or cache
(matchapack's store, Yarn's cache).

  shared-blocks.py <tree> <cache>

Counts every regular file below a node_modules directory of <tree>: a file
whose first extent (device and physical offset) is also the first extent of
a regular file in <cache> shares it (a clone of that file). macOS reads the
extent with fcntl F_LOG2PHYS_EXT, Linux with the FIEMAP ioctl (btrfs, XFS).
Empty files have no blocks and are counted apart; so are files whose data
has no block of its own (Linux: btrfs keeps small files inline in its
metadata, and a clone of such a file is a copy, never shared).
Prints one line:
"shared <n> of <m> files, <x> of <y> MiB (<e> empty)", with
", <i> inline" inside the parentheses when there are such files.
"""
import fcntl, os, struct, sys

F_LOG2PHYS_EXT = 65  # macOS
FS_IOC_FIEMAP = 0xC020660B  # Linux: _IOWR('f', 11, struct fiemap)
FIEMAP_FLAG_SYNC = 0x1
FIEMAP_HEAD = "=QQIIII"  # fm_start, fm_length, fm_flags, fm_mapped_extents, fm_extent_count, fm_reserved
FIEMAP_EXTENT = "=QQQQQIIII"  # fe_logical, fe_physical, fe_length, reserved64[2], fe_flags, reserved[3]
# The extent's data has no block address of its own, or none yet.
FIEMAP_EXTENT_NO_ADDRESS = 0x2 | 0x4 | 0x200  # UNKNOWN, DELALLOC, DATA_INLINE


class Inline(Exception):
    pass


def first_block(p):
    """(device, physical offset of the first block): offsets are per device."""
    fd = os.open(p, os.O_RDONLY)
    try:
        dev = os.fstat(fd).st_dev
        if sys.platform == "darwin":
            out = fcntl.fcntl(fd, F_LOG2PHYS_EXT, struct.pack("=Iqq", 0, 1, 0))
            return dev, struct.unpack("=Iqq", out)[2]
        buf = bytearray(struct.pack(FIEMAP_HEAD, 0, 0xFFFFFFFFFFFFFFFF, FIEMAP_FLAG_SYNC, 0, 1, 0))
        buf += bytes(struct.calcsize(FIEMAP_EXTENT))
        fcntl.ioctl(fd, FS_IOC_FIEMAP, buf, True)
        mapped = struct.unpack_from(FIEMAP_HEAD, buf)[3]
        if mapped == 0:
            raise Inline()  # a hole only: no block
        ext = struct.unpack_from(FIEMAP_EXTENT, buf, struct.calcsize(FIEMAP_HEAD))
        if ext[5] & FIEMAP_EXTENT_NO_ADDRESS:
            raise Inline()
        return dev, ext[1]
    finally:
        os.close(fd)


def files(root, under_node_modules):
    for d, dirs, names in os.walk(root):
        if under_node_modules and "node_modules" not in d[len(root):].split(os.sep):
            continue
        for n in names:
            p = os.path.join(d, n)
            st = os.lstat(p)
            if (st.st_mode & 0o170000) == 0o100000:
                yield p, st.st_size


def main():
    tree, cache = sys.argv[1], sys.argv[2]
    offsets = set()
    for p, size in files(cache, False):
        if size > 0:
            try:
                offsets.add(first_block(p))
            except (OSError, Inline):
                pass
    n = m = empty = inline = 0
    shared = total = 0
    for p, size in files(tree, True):
        if size == 0:
            empty += 1
            continue
        m += 1
        total += size
        try:
            if first_block(p) in offsets:
                n += 1
                shared += size
        except Inline:
            inline += 1
        except OSError:
            pass
    mib = 1024 * 1024
    extra = f", {inline} inline" if inline else ""
    print(f"shared {n} of {m} files, {shared / mib:.1f} of {total / mib:.1f} MiB ({empty} empty{extra})")


main()

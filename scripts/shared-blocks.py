#!/usr/bin/env python3
"""How much of a tree's node_modules shares its blocks with a Yarn cache.

  shared-blocks.py <tree> <cache>

Counts every regular file below a node_modules directory of <tree>: a file
whose first extent (device and physical offset, macOS fcntl F_LOG2PHYS_EXT)
is also the first extent of a regular file in <cache> shares it (a clone of
that file).
Empty files have no blocks and are counted apart. Prints one line:
"shared <n> of <m> files, <x> of <y> MiB (<e> empty)". macOS only.
"""
import fcntl, os, struct, sys

F_LOG2PHYS_EXT = 65


def first_block(p):
    """(device, physical offset of the first block): offsets are per device."""
    fd = os.open(p, os.O_RDONLY)
    try:
        out = fcntl.fcntl(fd, F_LOG2PHYS_EXT, struct.pack("=Iqq", 0, 1, 0))
        return os.fstat(fd).st_dev, struct.unpack("=Iqq", out)[2]
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
            except OSError:
                pass
    n = m = empty = 0
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
        except OSError:
            pass
    mib = 1024 * 1024
    print(f"shared {n} of {m} files, {shared / mib:.1f} of {total / mib:.1f} MiB ({empty} empty)")


main()

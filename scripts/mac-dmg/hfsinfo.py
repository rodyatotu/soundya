#!/usr/bin/env python3
"""Reads the HFS+ side of a hybrid image made by xorriso (-hfsplus): the
volume's name and creation date, and every catalog entry with its CNID,
parent, BSD mode and Finder type/creator. build_dmg.py needs the CNIDs for the
background picture's alias; the test checks the Applications symlink and the
app's executable bits with it.

    python3 scripts/mac-dmg/hfsinfo.py image.dmg
"""
import struct
import sys


def partition_offset(f):
    """Byte offset of the Apple_HFS partition (APM), or 0 for a bare volume."""
    f.seek(0)
    b0 = f.read(512)
    if b0[:2] != b"ER":
        return 0
    block = struct.unpack(">H", b0[2:4])[0]
    i = 1
    while True:
        f.seek(i * block)
        e = f.read(block)
        if e[:2] != b"PM":
            return 0
        count, start, _size = struct.unpack(">III", e[4:16])
        ptype = e[48:80].split(b"\0")[0]
        if ptype == b"Apple_HFS":
            return start * block
        if i >= count:
            return 0
        i += 1


def read(path):
    f = open(path, "rb")
    base = partition_offset(f)
    f.seek(base + 1024)
    vh = f.read(512)
    if vh[:2] not in (b"H+", b"HX"):
        raise SystemExit("no HFS+ volume")
    create_date = struct.unpack(">I", vh[16:20])[0]
    block_size = struct.unpack(">I", vh[40:44])[0]
    cat = vh[272:352]
    extents = [struct.unpack(">II", cat[16 + 8 * k:24 + 8 * k]) for k in range(8)]

    def cat_read(offset, n):
        out = b""
        pos = 0
        for start, count in extents:
            length = count * block_size
            if offset < pos + length and count:
                f.seek(base + start * block_size + (offset - pos))
                take = min(n - len(out), pos + length - offset)
                out += f.read(take)
                offset += take
                if len(out) >= n:
                    break
            pos += length
        return out

    header = cat_read(0, 512)
    first_leaf = struct.unpack(">I", header[24:28])[0]
    node_size = struct.unpack(">H", header[32:34])[0]
    entries = []
    node = first_leaf
    while node:
        nd = cat_read(node * node_size, node_size)
        flink, _blink, kind, _h, nrec = struct.unpack(">IIbBH", nd[:12])
        if kind != -1:
            break
        offs = [struct.unpack(">H", nd[node_size - 2 * (k + 1):node_size - 2 * k])[0] for k in range(nrec)]
        for o in offs:
            klen = struct.unpack(">H", nd[o:o + 2])[0]
            parent = struct.unpack(">I", nd[o + 2:o + 6])[0]
            nlen = struct.unpack(">H", nd[o + 6:o + 8])[0]
            name = nd[o + 8:o + 8 + 2 * nlen].decode("utf-16-be")
            d = o + 2 + klen
            d += d % 2
            rtype = struct.unpack(">h", nd[d:d + 2])[0]
            if rtype not in (1, 2):
                continue
            cnid = struct.unpack(">I", nd[d + 8:d + 12])[0]
            mode = struct.unpack(">H", nd[d + 42:d + 44])[0]
            ftype, creator = (nd[d + 48:d + 52], nd[d + 52:d + 56]) if rtype == 2 else (b"", b"")
            entries.append({"name": name, "parent": parent, "cnid": cnid, "folder": rtype == 1, "mode": mode,
                            "type": ftype, "creator": creator})
        node = flink
    # The volume's name is the root folder's name (parent 1).
    root = next((e for e in entries if e["parent"] == 1), None)
    return {"name": root["name"] if root else "", "create_date": create_date, "entries": entries}


def path_of(info, entry):
    by_id = {e["cnid"]: e for e in info["entries"]}
    parts = [entry["name"]]
    p = entry["parent"]
    while p in by_id and by_id[p]["parent"] != 1:
        parts.append(by_id[p]["name"])
        p = by_id[p]["parent"]
    return "/" + "/".join(reversed(parts))


if __name__ == "__main__":
    info = read(sys.argv[1])
    print("volume:", info["name"], "created:", info["create_date"])
    for e in info["entries"]:
        if e["parent"] == 1:
            continue
        print(f"{e['cnid']:>4} {oct(e['mode']):>9} {e['type'].decode(errors='replace'):4} {e['creator'].decode(errors='replace'):4} {path_of(info, e)}")

"""Synthetic game + two AMPRCMD1 journals for the profile generator's reference test.
Index written in AMPRIDX3 (the layout tools/build_ampr_index.py writes); journals use
AprReadFile, AprReadGather/Scatter and a few non-read commands so the decoder's skipping is
exercised. Deterministic (seeded)."""
import os, random, struct, sys

out = sys.argv[1]
os.makedirs(out, exist_ok=True)

# (relative path, size)
files = [
    ("eboot.bin", 3_000_000),
    ("sce_module/libc.prx", 400_000),
    ("sce_sys/param.json", 2_000),
    ("fakelib/libSceAmpr.sprx", 600_000),
    ("data/level4/world_4.dat2", 48 << 20),
    ("data/level5/world_5.dat2", 6 << 20),
    ("data/level6/world_6.dat2", 24 << 20),
    ("data/ui/strings.bin", 300_000),
    ("audio/bank.wem", 5 << 20),
    ("video/intro.bik", 9 << 20),
] + [(f"data/tex/t{i:03d}.dds", 256 << 10) for i in range(40)] + [
    (f"data/meta/m{i:03d}.idx", 4096) for i in range(120)
]


def fnv1a64(b):
    h = 1469598103934665603
    for x in b:
        h = ((h ^ x) * 1099511628211) & 0xFFFFFFFFFFFFFFFF
    return h or 1


def fold(h):
    return h


def write_index(path, entries, mtime=1700000000):
    # AMPRIDX3: header <8sIIQQQII>, records <IIQq>, path blob, hash table <QII> (2x slots).
    blob = b""
    recs = []
    for rel, size in entries:
        p = ("/app0/" + rel).encode()
        recs.append((len(blob), len(p), size, mtime))
        blob += p + b"\0"
    n = len(recs)
    slots = 2
    while slots < n * 2:
        slots *= 2
    table = [(0, 0, 0)] * slots
    mask = slots - 1
    for i, (rel, _) in enumerate(entries):
        key = rel.lower().encode()
        h = fnv1a64(key)
        pos = h & mask
        dup = 0
        while table[pos][1] != 0:
            if table[pos][0] == h:
                dup = 1
                table[pos] = (table[pos][0], table[pos][1], table[pos][2] | 1)
            pos = (pos + 1) & mask
        table[pos] = (h, i + 1, dup)
    header_size = struct.calcsize("<8sIIQQQII")
    path_end = header_size + n * 24 + len(blob)
    hash_offset = (path_end + 15) & ~15
    with open(path, "wb") as f:
        f.write(struct.pack("<8sIIQQQII", b"AMPRIDX3", 3, 24, n, len(blob), hash_offset, 16, slots))
        for r in recs:
            f.write(struct.pack("<IIQq", *r))
        f.write(blob)
        f.write(b"\0" * (hash_offset - path_end))
        for h, idx, flags in table:
            f.write(struct.pack("<QII", h, idx, flags))


ids = {rel: (i + 1, size) for i, (rel, size) in enumerate(files)}


def read_file(file_id, offset, length):
    assert length >= 1
    w0 = 40 | (5 << 8) | ((offset & 0x3FFFF) << 12)
    return struct.pack("<6I", w0, length - 1, file_id & 0x7FFFFFFF, 0x10000000, (offset & 0xFFFC0000) & 0xFFFFFFFF, (offset >> 32) & 0xFF)


def read_gather(offset, length):
    w0 = 41 | (2 << 8) | ((offset & 0x3FFFF) << 12)
    return struct.pack("<3I", w0, length - 1, (offset >> 18) & 0x3FFFFF)


def read_scatter(length):
    return struct.pack("<3I", 0x22A, length - 1, 0)


def reset_gs():
    return struct.pack("<I", 47)


def wait_on_counter():
    # opcode 2, 1 dword
    return struct.pack("<I", 2 | (0 << 8))


def marker(text):
    payload = text.encode()[:12].ljust(12, b"\0")
    return struct.pack("<I", 0x5452000F | (1 << 12) | (3 << 8)) + payload


def journal(path, batches):
    with open(path, "wb") as f:
        seq = 1
        for prio, cmds in batches:
            payload = b"".join(cmds)
            header = struct.pack("<8sHHIQQQQQQIIIIIIII", b"AMPRCMD1", 1, 96, 96 + len(payload), seq, 0, seq * 1000,
                                 seq, 0x200000000, fnv1a64(payload), len(payload), len(payload), len(cmds), prio, 1, 0, 0, 0)
            f.write(header + payload)
            seq += 1


def session(seed, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    rng = random.Random(seed)
    batches = []
    # startup: strings read whole, metadata files read many times (hot, random)
    fid, size = ids["data/ui/strings.bin"]
    batches.append((0, [marker("startup"), read_file(fid, 0, size)]))
    for _ in range(3):
        cmds = []
        for i in rng.sample(range(120), 60):
            fid, size = ids[f"data/meta/m{i:03d}.idx"]
            cmds.append(read_file(fid, 0, size))
        batches.append((0, cmds))
    # streaming world 4: sequential 512 KiB with gather/scatter continuation
    fid, size = ids["data/level4/world_4.dat2"]
    off = 0
    while off + (512 << 10) <= size:
        cmds = [read_file(fid, off, 512 << 10)]
        for _ in range(3):
            off += 512 << 10
            if off + (512 << 10) > size:
                break
            cmds.append(read_scatter(512 << 10))
        cmds.append(reset_gs())
        off += 512 << 10
        batches.append((1, cmds))
    # world 6: 1 MiB reads, mostly sequential with some jumps
    fid, size = ids["data/level6/world_6.dat2"]
    off = 0
    for _ in range(40):
        if rng.random() < 0.15:
            off = rng.randrange(0, size - (1 << 20)) & ~0xFFFF
        length = min(1 << 20, size - off)
        batches.append((2, [read_file(fid, off, length), wait_on_counter()]))
        off = (off + length) % (size - (1 << 20))
    # world 5: repeated random 2 KiB reads (hot, random, 16 KiB blocks)
    fid, size = ids["data/level5/world_5.dat2"]
    hot = [rng.randrange(0, 2 << 20) & ~0xFFF for _ in range(150)]
    for _ in range(4):
        batches.append((0, [read_file(fid, o, 2048) for o in hot[:32]]))
        batches.append((0, [read_file(fid, o, 2048) for o in hot[32:64]]))
        batches.append((0, [read_file(fid, o, 2048) for o in hot[64:]]))
    # textures: each read once, 256 KiB whole, some twice
    for i in range(40):
        fid, size = ids[f"data/tex/t{i:03d}.dds"]
        batches.append((3, [read_file(fid, 0, size)]))
        if i % 5 == 0:
            batches.append((3, [read_gather(0, 128 << 10)] if False else [read_file(fid, 0, 128 << 10)]))
    # a gather after a file read: the state carries the file id
    fid, size = ids["data/level4/world_4.dat2"]
    batches.append((4, [read_file(fid, 0, 64 << 10), read_gather(1 << 20, 64 << 10), read_scatter(64 << 10), reset_gs()]))
    # audio and video: a few reads each, must stay store/loose by suffix
    fid, size = ids["audio/bank.wem"]
    batches.append((5, [read_file(fid, o, 4096) for o in range(0, 8 * 4096, 4096)]))
    fid, size = ids["video/intro.bik"]
    batches.append((5, [read_file(fid, o, 1 << 20) for o in range(0, 4 << 20, 1 << 20)]))
    # eboot read (known loose)
    fid, size = ids["eboot.bin"]
    batches.append((0, [read_file(fid, 0, 65536)]))
    # an AMM record (domain 2) the reader must skip
    write_index(os.path.join(out_dir, "ampr_emu.index"), files)
    journal(os.path.join(out_dir, "ampr_commands.bin"), batches)


session(1, os.path.join(out, "run1"))
session(2, os.path.join(out, "run2"))
print("ok")

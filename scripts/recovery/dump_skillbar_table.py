"""Dump the original Reffect skillbar slot table (byte_1801B0AD8).

The original collector `sub_1800E2EC0` walks a static 12-byte-stride table of
(raw slot, sub index, destination slot) entries and calls the ability reader
`sub_1800E1BB0` for each of them. The table is the ground truth for which
combatant slots the original publishes, so it can be compared against the open
reader's `slot_map()`.

Usage: python dump_skillbar_table.py [path/to/Reffect.dll]
"""

import struct
import sys

TABLE_RVA = 0x1B0AD8
ENTRY_SIZE = 12
ENTRY_COUNT = 17
IMAGE_BASE = 0x180000000


def sections(data):
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    assert data[pe_offset : pe_offset + 4] == b"PE\0\0", "not a PE file"
    section_count = struct.unpack_from("<H", data, pe_offset + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe_offset + 20)[0]
    offset = pe_offset + 24 + optional_size
    result = []
    for index in range(section_count):
        base = offset + index * 40
        name = data[base : base + 8].rstrip(b"\0").decode("ascii", "replace")
        virtual_size, virtual_address, raw_size, raw_offset = struct.unpack_from(
            "<IIII", data, base + 8
        )
        result.append((name, virtual_address, virtual_size, raw_offset, raw_size))
    return result


def read_rva(data, rva, size):
    for name, virtual_address, virtual_size, raw_offset, raw_size in sections(data):
        if virtual_address <= rva < virtual_address + max(virtual_size, raw_size):
            delta = rva - virtual_address
            if delta + size > raw_size:
                raise SystemExit(f"RVA {rva:#x} crosses the raw end of {name}")
            start = raw_offset + delta
            return name, data[start : start + size]
    raise SystemExit(f"RVA {rva:#x} is not mapped by any section")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "Reffect.dll"
    with open(path, "rb") as handle:
        data = handle.read()
    section, table = read_rva(data, TABLE_RVA, ENTRY_SIZE * ENTRY_COUNT)
    print(f"{path}: table {IMAGE_BASE + TABLE_RVA:#x} in {section}")
    print(f"{'idx':>3} {'slot':>5} {'sub':>4} {'dest':>5}")
    for index in range(ENTRY_COUNT):
        slot, sub, dest = struct.unpack_from("<IIB", table, index * ENTRY_SIZE)
        print(f"{index:>3} {slot:>5} {sub:>4} {dest:>5}")


if __name__ == "__main__":
    main()

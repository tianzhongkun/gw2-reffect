"""Export callers that reference one virtual address in an existing IDA DB.

Usage:
    idat.exe -A -S"scripts/recovery/export_ida_xrefs.py 0x180190230 out.json" sample.i64

The address may be an import-address-table slot, a function, or data.  The
script reads the existing database and writes an offline JSON report; it never
changes names, types, comments, or bytes in the database.
"""

from __future__ import annotations

import json
import os

import ida_auto
import ida_funcs
import ida_hexrays
import ida_name
import ida_nalt
import ida_pro
import idautils
import idc


def function_record(address: int) -> dict[str, object]:
    """Return the containing function and optional Hex-Rays pseudocode."""
    function = ida_funcs.get_func(address)
    if function is None:
        return {
            "address": f"0x{address:X}",
            "function": None,
        }

    record: dict[str, object] = {
        "address": f"0x{address:X}",
        "function": {
            "address": f"0x{function.start_ea:X}",
            "rva": function.start_ea - ida_nalt.get_imagebase(),
            "name": ida_name.get_name(function.start_ea),
            "size": function.end_ea - function.start_ea,
        },
    }
    try:
        record["pseudocode"] = str(ida_hexrays.decompile(function.start_ea))
    except ida_hexrays.DecompilationFailure:
        record["pseudocode"] = None
    return record


def main() -> None:
    if len(idc.ARGV) != 3:
        raise SystemExit("expected target address and output JSON path")

    target = int(idc.ARGV[1], 0)
    output = os.path.abspath(idc.ARGV[2])
    ida_auto.auto_wait()
    ida_hexrays.init_hexrays_plugin()

    xrefs = []
    for xref in idautils.XrefsTo(target):
        record = function_record(xref.frm)
        record["from"] = f"0x{xref.frm:X}"
        record["type"] = xref.type
        xrefs.append(record)

    with open(output, "w", encoding="utf-8") as file:
        json.dump(
            {
                "target": f"0x{target:X}",
                "xrefs": xrefs,
            },
            file,
            ensure_ascii=False,
            indent=2,
        )
    ida_pro.qexit(0)


if __name__ == "__main__":
    main()

"""Find functions that reference a string in an IDA database.

Usage:
    idat.exe -A -S"scripts/recovery/find_ida_string_xrefs.py <needle> <output.json>" sample.dll

The output includes the matching string address, each xref's containing
function, and Hex-Rays pseudocode when available.
"""

from __future__ import annotations

import json
import os

import ida_auto
import ida_bytes
import ida_funcs
import ida_hexrays
import ida_name
import ida_nalt
import ida_pro
import idautils
import idc


def function_record(ea: int) -> dict[str, object]:
    func = ida_funcs.get_func(ea)
    assert func is not None

    record: dict[str, object] = {
        "address": f"0x{func.start_ea:X}",
        "rva": func.start_ea - ida_nalt.get_imagebase(),
        "name": ida_name.get_name(func.start_ea),
        "size": func.end_ea - func.start_ea,
    }
    try:
        record["pseudocode"] = str(ida_hexrays.decompile(func.start_ea))
    except ida_hexrays.DecompilationFailure:
        pass
    return record


def main() -> None:
    if len(idc.ARGV) != 3:
        raise SystemExit("expected string needle and output JSON path")

    needle = idc.ARGV[1].lower()
    output = os.path.abspath(idc.ARGV[2])
    ida_auto.auto_wait()
    ida_hexrays.init_hexrays_plugin()

    matches: list[dict[str, object]] = []
    seen_functions: set[int] = set()
    for string in idautils.Strings():
        value = str(string)
        if needle not in value.lower():
            continue

        xrefs = []
        for xref in idautils.XrefsTo(string.ea):
            func = ida_funcs.get_func(xref.frm)
            if func is None:
                continue
            if func.start_ea not in seen_functions:
                seen_functions.add(func.start_ea)
                xrefs.append(function_record(func.start_ea))

        matches.append(
            {
                "address": f"0x{string.ea:X}",
                "rva": string.ea - ida_nalt.get_imagebase(),
                "value": value,
                "functions": xrefs,
            }
        )

    with open(output, "w", encoding="utf-8") as file:
        json.dump(matches, file, ensure_ascii=False, indent=2)
    ida_pro.qexit(0)


if __name__ == "__main__":
    main()

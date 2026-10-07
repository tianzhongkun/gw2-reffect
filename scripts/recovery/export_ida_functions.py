"""Export IDA function metadata and Hex-Rays pseudocode for offline analysis.

Usage:
    idat.exe -A -S"scripts/recovery/export_ida_functions.py <output.json>" sample.dll

The script is intentionally independent of a particular binary.  It emits only
analysis metadata and source-like pseudocode for functions Hex-Rays can
decompile, so a recovery can be repeated against a later release.
"""

from __future__ import annotations

import json
import os
import sys

import ida_auto
import ida_funcs
import ida_hexrays
import ida_name
import ida_nalt
import ida_pro
import idautils


def function_record(ea: int) -> dict[str, object]:
    func = ida_funcs.get_func(ea)
    assert func is not None

    record: dict[str, object] = {
        "address": f"0x{ea:X}",
        "rva": ea - ida_nalt.get_imagebase(),
        "name": ida_name.get_name(ea),
        "size": func.end_ea - func.start_ea,
    }

    try:
        pseudocode = str(ida_hexrays.decompile(ea))
    except ida_hexrays.DecompilationFailure:
        pseudocode = None

    if pseudocode is not None:
        record["pseudocode"] = pseudocode
    return record


def main() -> None:
    if len(idc.ARGV) != 2:
        raise SystemExit("expected output JSON path")

    output = os.path.abspath(idc.ARGV[1])
    ida_auto.auto_wait()
    if not ida_hexrays.init_hexrays_plugin():
        raise SystemExit("Hex-Rays decompiler is unavailable")

    functions = [function_record(ea) for ea in idautils.Functions()]
    with open(output, "w", encoding="utf-8") as file:
        json.dump(functions, file, ensure_ascii=False, indent=2)

    ida_pro.qexit(0)


if __name__ == "__main__":
    import idc

    main()

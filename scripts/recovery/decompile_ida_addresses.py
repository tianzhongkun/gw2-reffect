"""Export selected IDA/Hex-Rays functions for repeatable private-backend recovery.

Usage:
    idat.exe -A -S"scripts/recovery/decompile_ida_addresses.py out.json 0x1800D5AF0" sample.i64

The script opens an existing IDA database, reads only the requested functions,
and emits their metadata and pseudocode.  It never applies names, types, or
comments to the database.
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
import idc


def function_record(address: int) -> dict[str, object]:
    """Return independently useful metadata for one selected function."""
    function = ida_funcs.get_func(address)
    if function is None:
        return {"address": f"0x{address:X}", "error": "function not found"}

    record: dict[str, object] = {
        "address": f"0x{function.start_ea:X}",
        "rva": function.start_ea - ida_nalt.get_imagebase(),
        "name": ida_name.get_name(function.start_ea),
        "size": function.end_ea - function.start_ea,
    }
    try:
        record["pseudocode"] = str(ida_hexrays.decompile(function.start_ea))
    except ida_hexrays.DecompilationFailure:
        record["pseudocode"] = None
    return record


def main() -> None:
    if len(idc.ARGV) < 3:
        raise SystemExit("expected output path followed by one or more function addresses")

    output = os.path.abspath(idc.ARGV[1])
    addresses = [int(argument, 0) for argument in idc.ARGV[2:]]
    ida_auto.auto_wait()
    if not ida_hexrays.init_hexrays_plugin():
        raise SystemExit("Hex-Rays decompiler is unavailable")

    with open(output, "w", encoding="utf-8") as file:
        json.dump(
            [function_record(address) for address in addresses],
            file,
            ensure_ascii=False,
            indent=2,
        )
    ida_pro.qexit(0)


if __name__ == "__main__":
    main()

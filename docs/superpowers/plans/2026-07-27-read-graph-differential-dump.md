# Read-Graph Differential Dump Implementation Plan

> **For agentic workers:** Execute each checked task with its stated test cycle. The repository is deliberately dirty; do not commit, deploy, or change production Rust code as part of this diagnostic tool.

**Goal:** Produce a deterministic, read-only comparison of the original and Open addon reader snapshots that reports the first meaningful field differences in equipment, Buffs, resources, and skill state.

**Architecture:** Keep capture and comparison separate. `dump_reffect_read_graph.py` remains the raw pointer-chain capture format; a new offline comparator reduces each raw graph to stable semantic fields, then recursively compares those fields. The comparator neither opens a process nor writes game memory.

**Tech Stack:** Python 3 standard library, `unittest`, JSON.

## Global Constraints

- Inputs are previously captured `reffect-read-graph/v1` JSON documents.
- Ignore process-specific addresses, raw byte strings, capture timestamps, and pointer chains in the semantic comparison.
- Treat `Foreign` effect variants `0x12` (direct) and `0x17` (wrapped) as the same equipped-effect result when their effect IDs match.
- Include only active Buff nodes (`state == 1`) in the semantic Buff list and tolerate short-duration drift up to 250 ms.
- Do not use debugger breakpoints, process writes, code injection, deployment, or commits.

---

### Task 1: Define semantic extraction and diff contracts

**Files:**

- Create: `scripts/recovery/test_compare_reffect_read_graph.py`
- Create: `scripts/recovery/compare_reffect_read_graph.py`

**Interfaces:**

- Produces `summarize_read_graph(graph: dict[str, object]) -> dict[str, object]`.
- Produces `diff_summaries(reference: dict[str, object], open_graph: dict[str, object], *, duration_tolerance_ms: int = 250) -> list[dict[str, object]]`.
- A difference is `{"path": str, "reference": object, "open": object}` and is deterministically sorted by path.

- [ ] **Step 1: Write a failing test for direct and wrapped equipped effects**

```python
summary = summarize_read_graph(graph_with_direct_and_wrapped_effects())
assert summary["gear"] == {"sigils": [21824, 9452, 0, 0], "relic": 74494}
```

- [ ] **Step 2: Run the test and verify RED**

Run: `python -m unittest scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: failure because `compare_reffect_read_graph` does not exist.

- [ ] **Step 3: Implement the minimal extractor**

Create `summarize_read_graph` with a small helper that unwraps a trace entry's `value`, extracts direct/wrapped equipment IDs in raw-slot order, and preserves only race, profession, selected resource values, skill IDs, and active Buffs.

- [ ] **Step 4: Run the test and verify GREEN**

Run: `python -m unittest scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: direct and wrapped effects resolve to the same semantic gear representation.

### Task 2: Diff only meaningful reader results

**Files:**

- Modify: `scripts/recovery/test_compare_reffect_read_graph.py`
- Modify: `scripts/recovery/compare_reffect_read_graph.py`

**Interfaces:**

- `diff_summaries` compares scalar fields, sorted skill maps, and active Buff identities.
- Duration deltas no larger than `duration_tolerance_ms` produce no difference.

- [ ] **Step 1: Write failing tests for active Buff filtering, duration tolerance, and a missing Weapon Swap result**

```python
differences = diff_summaries(reference, open_graph)
assert {item["path"] for item in differences} == {
    "buffs.active", "gear.relic", "skillbar.weapon_swap"
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run: `python -m unittest scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: failure because the comparator has no diff implementation and no duration tolerance.

- [ ] **Step 3: Implement a recursive deterministic comparison**

Compare mappings by key and lists by canonical ordering. For active Buff durations, compare effect/state identity first and suppress only duration-only differences within tolerance. Report absent values explicitly as `null`.

- [ ] **Step 4: Run the tests and verify GREEN**

Run: `python -m unittest scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: only missing/effectively different fields are reported, in lexical path order.

### Task 3: Expose an offline command-line report

**Files:**

- Modify: `scripts/recovery/test_compare_reffect_read_graph.py`
- Modify: `scripts/recovery/compare_reffect_read_graph.py`

**Interfaces:**

- CLI: `python scripts/recovery/compare_reffect_read_graph.py --reference original.json --open open.json --out diff.json`.
- The rendered JSON has `format`, `reference`, `open`, and `differences` keys.
- Exit code is `0` when snapshots match and `1` when any difference is present.

- [ ] **Step 1: Write a failing CLI integration test using two temporary JSON files**

```python
exit_code = main(["--reference", str(reference), "--open", str(open_graph), "--out", str(report)])
assert exit_code == 1
assert json.loads(report.read_text())["differences"][0]["path"] == "gear.relic"
```

- [ ] **Step 2: Run the tests and verify RED**

Run: `python -m unittest scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: failure because `main` does not parse inputs or produce a report.

- [ ] **Step 3: Implement the command-line wrapper**

Load the two JSON files, generate semantic summaries and the diff, print or atomically write the JSON report, then return the documented exit code. Do not create output folders or modify the source snapshots.

- [ ] **Step 4: Run the full diagnostic-script suite**

Run: `python -m unittest scripts/recovery/test_dump_reffect_read_graph.py scripts/recovery/test_compare_reffect_read_graph.py -v`

Expected: every test passes.

## Self-Review

- Captures remain raw/read-only and the comparator owns all semantic reduction.
- Tests cover the two observed `Foreign` branches, active Buff selection, duration race tolerance, missing Weapon Swap reporting, stable ordering, and CLI output.
- No part of this plan alters the game process, the addon DLL, deployment folder, or unrelated user changes.

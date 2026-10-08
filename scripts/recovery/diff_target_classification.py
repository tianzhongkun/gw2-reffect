#!/usr/bin/env python3
"""Recover the target classification offsets from the addon's probe log.

The open reader cannot yet classify a target as player/NPC/monster/rank/node
because the client fields carrying the agent type, disposition, NPC rank and
gathering discipline are not part of the recovered read graph. The addon
therefore dumps the candidate structs once per selected target:

    [reffect-target-classify] kind=0 descriptor=0x... object=0x... character=0x... agent=0x... core=0x...
    [reffect-target-classify] descriptor[0x00..0x40]=00000000 ...
    [reffect-target-classify] object[0x00..0x80]=...
    [reffect-target-classify] character[0x300..0x540]=...
    [reffect-target-classify] core[0x000..0x180]=...
    [reffect-target-classify] agent[0x000..0x140]=...

Select targets whose category you know in game and diff the dumps:

    python scripts/recovery/diff_target_classification.py \
        --log Nexus.log \
        --labels normal,normal,veteran,veteran,champion,champion

**Select each target at least twice** and repeat its label: with only one sample
per label, any field that merely counts samples (a timer, an id, an address)
looks like a perfect separator. Two samples per label make those fail the
per-label consistency check.

A field that is constant within each label but differs between labels is a
candidate for the classification offset. Candidates are ranked by whether they
are consistent per label, then by how many distinct values they take across the
labels, then by how small the values are (enums are small; pointers, timers and
ids are not).

This script only reads a log file: it never opens the game process and never
writes to it.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

PROBE_PREFIX = "[reffect-target-classify]"

# `descriptor`/`object` are printed as `0x` hex. The optional pointers were
# originally printed with `{:?}`, i.e. `Some(<decimal>)`/`None`, and newer builds
# print `0x` hex, so accept both forms.
OPTIONAL_POINTER = r"(?:Some\(0x[0-9a-f]+\)|Some\(\d+\)|0x[0-9a-f]+|None)"

HEADER = re.compile(
    r"kind=(?P<kind>\d+)"
    r" descriptor=(?P<descriptor>0x[0-9a-f]+)"
    r" object=(?P<object>0x[0-9a-f]+)"
    r" character=(?:Some\((?P<character>\d+)\)|(?P<character_hex>0x[0-9a-f]+)|None)"
    r" agent=" + OPTIONAL_POINTER
    + r" core=" + OPTIONAL_POINTER
)

REGION = re.compile(
    r"^(?P<region>[a-z]+)\[(?P<start>0x[0-9a-f]+)\.\.(?P<end>0x[0-9a-f]+)\]=(?P<words>.*)$"
)

UNREADABLE = "????????"

# Values at or above this are pointers, addresses, timers or ids rather than an
# enum value, so they are not useful candidates by default.
DEFAULT_MAX_VALUE = 0x1000


@dataclass
class Sample:
    """One probe dump, i.e. one selected target."""

    kind: int
    object: int
    label: str | None = None
    # region name -> absolute struct offset -> value (None when unreadable)
    regions: dict[str, dict[int, int | None]] = field(default_factory=dict)

    def offsets(self) -> dict[tuple[str, int], int | None]:
        return {
            (region, offset): value
            for region, words in self.regions.items()
            for offset, value in words.items()
        }


def parse_samples(text: str) -> list[Sample]:
    """Parses the probe lines out of a Nexus log.

    Lines that are not part of a probe block are ignored, so the log can be
    passed in verbatim.
    """
    samples: list[Sample] = []
    current: Sample | None = None

    for raw in text.splitlines():
        marker = raw.find(PROBE_PREFIX)
        if marker < 0:
            continue
        line = raw[marker + len(PROBE_PREFIX) :].strip()

        header = HEADER.search(line)
        if header:
            current = Sample(
                kind=int(header.group("kind")),
                object=int(header.group("object"), 16),
            )
            samples.append(current)
            continue

        region = REGION.match(line)
        if region and current is not None:
            start = int(region.group("start"), 16)
            words = region.group("words").split()
            current.regions[region.group("region")] = {
                start + index * 4: (None if word == UNREADABLE else int(word, 16))
                for index, word in enumerate(words)
            }

    return samples


def label_samples(samples: list[Sample], labels: list[str]) -> None:
    """Assigns labels to samples in order.

    Raises `ValueError` when the count does not match, because a wrong pairing
    would silently produce meaningless candidates.
    """
    if len(samples) != len(labels):
        raise ValueError(
            f"got {len(samples)} probe dumps but {len(labels)} labels; "
            "select each target exactly once, in the order of --labels"
        )
    for sample, label in zip(samples, labels, strict=True):
        sample.label = label


@dataclass
class Candidate:
    """An offset whose value separates the labels."""

    region: str
    offset: int
    distinct_label_values: int
    consistent: bool
    values: dict[str, int]

    @property
    def size(self) -> int:
        return max(self.values.values(), default=0)


def rank_candidates(
    samples: list[Sample],
    *,
    max_value: int = DEFAULT_MAX_VALUE,
) -> list[Candidate]:
    """Returns offsets that separate the labels, best candidate first.

    An offset is a candidate when it is readable in every sample, stays below
    `max_value` (so pointers, addresses and timers are excluded) and takes at
    least two distinct per-label values.

    Sample each target at least twice (repeat its label) so that fields which
    merely count samples - timers, ids, addresses - fail the per-label
    consistency check instead of looking like perfect separators.
    """
    if not samples:
        return []

    keys = set(samples[0].offsets())
    for sample in samples[1:]:
        keys &= set(sample.offsets())

    candidates: list[Candidate] = []
    for region, offset in sorted(keys):
        values: dict[str, set[int]] = {}
        for sample in samples:
            value = sample.offsets()[(region, offset)]
            if value is None or value >= max_value:
                break
            assert sample.label is not None
            values.setdefault(sample.label, set()).add(value)
        else:
            label_values = {
                label: next(iter(label_set))
                for label, label_set in values.items()
                if len(label_set) == 1
            }
            consistent = len(label_values) == len(values)
            distinct = len(set(label_values.values()))
            if distinct < 2:
                continue
            candidates.append(
                Candidate(
                    region=region,
                    offset=offset,
                    distinct_label_values=distinct,
                    consistent=consistent,
                    values=label_values,
                )
            )

    candidates.sort(
        key=lambda candidate: (
            not candidate.consistent,
            -candidate.distinct_label_values,
            candidate.size,
            candidate.region,
            candidate.offset,
        )
    )
    return candidates


def single_sample_labels(samples: list[Sample]) -> list[str]:
    """Returns the labels that were sampled only once."""
    counts: dict[str, int] = {}
    for sample in samples:
        if sample.label is not None:
            counts[sample.label] = counts.get(sample.label, 0) + 1
    return sorted(label for label, count in counts.items() if count == 1)


def render_report(samples: list[Sample], candidates: list[Candidate]) -> str:
    lines = [f"parsed {len(samples)} probe dump(s)"]

    for index, sample in enumerate(samples):
        regions = ", ".join(
            f"{region}[{len(words)}]" for region, words in sorted(sample.regions.items())
        )
        lines.append(
            f"  #{index + 1} label={sample.label} kind={sample.kind} "
            f"object={sample.object:#x} regions: {regions}"
        )

    once = single_sample_labels(samples)
    if once:
        lines.append("")
        lines.append(
            "warning: labels sampled only once cannot be told apart from fields "
            "that merely count samples: " + ", ".join(once)
        )
        lines.append(
            "         select each target at least twice (repeat its label) for a "
            "reliable result"
        )

    if not candidates:
        lines.append("")
        lines.append("no separating offset found")
        return "\n".join(lines)

    lines.append("")
    lines.append("candidates (constant per label, different across labels):")
    lines.append(f"  {'region':<11} {'offset':>7}  {'sep':>3} {'stable':>6}  values")
    for candidate in candidates[:40]:
        values = " ".join(
            f"{label}={candidate.values[label]:#x}" for label in candidate.values
        )
        lines.append(
            f"  {candidate.region:<11} {candidate.offset:#07x}  "
            f"{candidate.distinct_label_values:>3} "
            f"{'yes' if candidate.consistent else 'no':>6}  {values}"
        )

    return "\n".join(lines)


def report_dict(samples: list[Sample], candidates: list[Candidate]) -> dict[str, Any]:
    return {
        "format": "reffect-target-classification-diff/v1",
        "samples": [
            {
                "label": sample.label,
                "kind": sample.kind,
                "object": sample.object,
                "regions": {
                    region: {f"{offset:#x}": value for offset, value in words.items()}
                    for region, words in sorted(sample.regions.items())
                },
            }
            for sample in samples
        ],
        "candidates": [
            {
                "region": candidate.region,
                "offset": candidate.offset,
                "distinct_label_values": candidate.distinct_label_values,
                "consistent": candidate.consistent,
                "values": candidate.values,
            }
            for candidate in candidates
        ],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--log", required=True, type=Path, help="Nexus log containing the probe lines")
    parser.add_argument(
        "--labels",
        required=True,
        help="comma separated labels, one per probe dump in log order",
    )
    parser.add_argument("--out", type=Path, help="write the report as JSON")
    parser.add_argument(
        "--max-value",
        type=lambda value: int(value, 0),
        default=DEFAULT_MAX_VALUE,
        help="ignore values at or above this (default: 0x1000)",
    )
    args = parser.parse_args(argv)

    try:
        text = args.log.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        print(f"failed to read {args.log}: {error}", file=sys.stderr)
        return 2

    samples = parse_samples(text)
    if not samples:
        print(f"no {PROBE_PREFIX} lines found in {args.log}", file=sys.stderr)
        return 1

    labels = [label.strip() for label in args.labels.split(",") if label.strip()]
    try:
        label_samples(samples, labels)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 1

    candidates = rank_candidates(samples, max_value=args.max_value)
    print(render_report(samples, candidates))

    if args.out:
        args.out.write_text(
            json.dumps(report_dict(samples, candidates), indent=2),
            encoding="utf-8",
        )
        print(f"\nwrote {args.out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

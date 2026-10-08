"""Tests for the target classification offset diff tool."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from diff_target_classification import (  # noqa: E402
    Sample,
    label_samples,
    main,
    parse_samples,
    rank_candidates,
    render_report,
    single_sample_labels,
)


def probe_block(
    *,
    kind: int = 0,
    object_address: int = 0x2000_0000,
    character: int | None = 0x1000_0000,
    descriptor_words: list[int | None] | None = None,
    character_words: list[int | None] | None = None,
) -> list[str]:
    """Builds the log lines the addon probe emits for one target."""
    descriptor_words = descriptor_words if descriptor_words is not None else [0] * 16
    character_words = character_words if character_words is not None else [0] * 16

    def region(name: str, start: int, words: list[int | None]) -> str:
        end = start + len(words) * 4
        rendered = " ".join(
            "????????" if word is None else f"{word:08x}" for word in words
        )
        return f"[reffect-target-classify] {name}[{start:#04x}..{end:#04x}]={rendered}"

    character_field = "None" if character is None else f"{character:#x}"
    return [
        "[reffect-target-classify] "
        f"kind={kind} descriptor=0x30000000 object={object_address:#x} "
        f"character={character_field} agent=0x40000000 core=0x50000000",
        region("descriptor", 0x00, descriptor_words),
        region("object", 0x00, descriptor_words),
        region("character", 0x300, character_words),
        region("core", 0x00, descriptor_words),
        region("agent", 0x00, descriptor_words),
    ]


class ParseSamplesTest(unittest.TestCase):
    def test_parses_the_real_log_line_format(self) -> None:
        # Verbatim from Nexus.log (2026-10-08). The optional pointers were
        # printed with `{:?}`, i.e. `Some(<decimal>)`, which the first revision
        # of the parser rejected - it silently found no samples at all.
        text = (
            "2026-10-08 08:05:42.359  [Reffect Open]     [WARNING]   "
            "[reffect-target-classify] seq=1 kind=10 descriptor=0x1d09ba27b30 "
            "object=0x1d0bb889fc0 character=Some(1996011118456) "
            "agent=Some(1119788329) core=None"
        )

        samples = parse_samples(text)

        self.assertEqual(len(samples), 1)
        self.assertEqual(samples[0].kind, 10)
        self.assertEqual(samples[0].object, 0x1D0BB889FC0)

    def test_parses_hex_optional_pointers(self) -> None:
        text = (
            "[reffect-target-classify] seq=2 kind=0 descriptor=0x1d0aa6bcd30 "
            "object=0x1d0a43098b8 character=0x1d0a4309870 agent=0x1d0a4309b10 "
            "core=0x1d09b620000"
        )

        samples = parse_samples(text)

        self.assertEqual(len(samples), 1)
        self.assertEqual(samples[0].kind, 0)

    def test_parses_header_and_regions(self) -> None:
        text = "\n".join(
            [
                "some unrelated log line",
                *probe_block(character_words=[1, 2, 3, 4]),
            ]
        )

        samples = parse_samples(text)

        self.assertEqual(len(samples), 1)
        sample = samples[0]
        self.assertEqual(sample.kind, 0)
        self.assertEqual(sample.object, 0x2000_0000)
        self.assertEqual(
            sample.regions["character"],
            {0x300: 1, 0x304: 2, 0x308: 3, 0x30C: 4},
        )

    def test_attaches_labels_written_by_the_tagging_ui(self) -> None:
        text = "\n".join(
            [
                "[reffect-target-classify] seq=1 kind=0 descriptor=0x1 object=0x10 "
                "character=None agent=None core=None",
                "[reffect-target-classify] label=veteran seq=1",
                "[reffect-target-classify] seq=2 kind=0 descriptor=0x2 object=0x20 "
                "character=None agent=None core=None",
                "[reffect-target-classify] label=champion seq=2",
            ]
        )

        samples = parse_samples(text)

        self.assertEqual(
            [(sample.sequence, sample.label) for sample in samples],
            [(1, "veteran"), (2, "champion")],
        )

    def test_label_without_a_sequence_applies_to_the_last_sample(self) -> None:
        text = "\n".join(
            [
                "[reffect-target-classify] kind=0 descriptor=0x1 object=0x10 "
                "character=None agent=None core=None",
                "[reffect-target-classify] object[0x000..0x008]=00000001 ",
                "[reffect-target-classify] label=ore",
            ]
        )

        samples = parse_samples(text)

        self.assertEqual(samples[0].label, "ore")
        self.assertIsNone(samples[0].sequence)

    def test_merges_chunked_regions(self) -> None:
        # A wide region is emitted as several chunks. Replacing instead of
        # merging silently compares only the last chunk, which hid every field
        # outside it.
        text = "\n".join(
            [
                "[reffect-target-classify] seq=1 kind=0 descriptor=0x1 object=0x10 "
                "character=None agent=None core=None",
                "[reffect-target-classify] character[0x000..0x008]=00000001 00000002 ",
                "[reffect-target-classify] character[0x008..0x010]=00000003 00000004 ",
            ]
        )

        samples = parse_samples(text)

        self.assertEqual(
            samples[0].regions["character"],
            {0x000: 1, 0x004: 2, 0x008: 3, 0x00C: 4},
        )

    def test_keeps_unreadable_words_as_none(self) -> None:
        text = "\n".join(probe_block(character_words=[1, None, 3]))

        samples = parse_samples(text)

        self.assertIsNone(samples[0].regions["character"][0x304])

    def test_parses_multiple_targets_in_order(self) -> None:
        text = "\n".join(
            [
                *probe_block(character_words=[0xAA]),
                "noise",
                *probe_block(character_words=[0xBB]),
            ]
        )

        samples = parse_samples(text)

        self.assertEqual(len(samples), 2)
        self.assertEqual(samples[0].regions["character"][0x300], 0xAA)
        self.assertEqual(samples[1].regions["character"][0x300], 0xBB)

    def test_ignores_region_lines_without_a_header(self) -> None:
        text = "[reffect-target-classify] character[0x300..0x304]=00000001"

        self.assertEqual(parse_samples(text), [])


class LabelSamplesTest(unittest.TestCase):
    def test_assigns_labels_in_order(self) -> None:
        samples = [Sample(kind=0, object=1), Sample(kind=0, object=2)]

        label_samples(samples, ["normal", "veteran"])

        self.assertEqual([sample.label for sample in samples], ["normal", "veteran"])

    def test_rejects_a_count_mismatch(self) -> None:
        samples = [Sample(kind=0, object=1)]

        with self.assertRaises(ValueError):
            label_samples(samples, ["normal", "veteran"])


class RankCandidatesTest(unittest.TestCase):
    def test_finds_a_constant_per_label_rank_field(self) -> None:
        # Offset 0x304 holds the rank (1 normal, 3 veteran, 5 champion). Offset
        # 0x300 is a pointer and 0x308 a per-sample timer. Every label is
        # sampled twice, so the timer fails the consistency check.
        samples = []
        for index, (label, rank) in enumerate(
            [
                ("normal", 1),
                ("normal", 1),
                ("veteran", 3),
                ("veteran", 3),
                ("champion", 5),
                ("champion", 5),
            ]
        ):
            samples.append(
                Sample(
                    kind=0,
                    object=0x1000 + index,
                    label=label,
                    regions={
                        "character": {
                            0x300: 0x7FF0_0000,  # pointer, filtered by size
                            0x304: rank,
                            0x308: 1000 + index,  # timer, differs per sample
                        }
                    },
                )
            )

        candidates = rank_candidates(samples)

        offsets = [candidate.offset for candidate in candidates]
        self.assertEqual(offsets, [0x304])
        self.assertEqual(
            candidates[0].values,
            {"normal": 1, "veteran": 3, "champion": 5},
        )
        self.assertTrue(candidates[0].consistent)

    def test_single_sample_labels_are_reported(self) -> None:
        samples = [
            Sample(kind=0, object=index, label=label, regions={})
            for index, label in enumerate(["normal", "veteran", "veteran"])
        ]

        self.assertEqual(single_sample_labels(samples), ["normal"])
        self.assertIn(
            "labels sampled only once", render_report(samples, [])
        )

    def test_ignores_offsets_that_do_not_separate_labels(self) -> None:
        samples = [
            Sample(
                kind=0,
                object=0x1000 + index,
                label=label,
                regions={"character": {0x300: 7}},
            )
            for index, label in enumerate(["normal", "veteran"])
        ]

        self.assertEqual(rank_candidates(samples), [])

    def test_ignores_offsets_that_are_unreadable_in_any_sample(self) -> None:
        samples = [
            Sample(
                kind=0,
                object=0x1000 + index,
                label=label,
                regions={"character": {0x300: value}},
            )
            for index, (label, value) in enumerate(
                [("normal", 1), ("veteran", None), ("champion", 5)]
            )
        ]

        self.assertEqual(rank_candidates(samples), [])

    def test_prefers_consistent_offsets_over_noisy_ones(self) -> None:
        # 0x304 is the rank and holds one value per label. 0x300 still separates
        # the first two labels but is not constant for the third, so it ranks
        # below the rank field.
        samples = [
            Sample(
                kind=0,
                object=0x1000 + index,
                label=label,
                regions={"character": {0x300: noisy, 0x304: rank}},
            )
            for index, (label, rank, noisy) in enumerate(
                [
                    ("normal", 1, 10),
                    ("veteran", 5, 20),
                    ("champion", 9, 30),
                    ("champion", 9, 31),
                ]
            )
        ]

        candidates = rank_candidates(samples)

        self.assertEqual([candidate.offset for candidate in candidates], [0x304, 0x300])
        self.assertTrue(candidates[0].consistent)
        self.assertFalse(candidates[1].consistent)
        self.assertEqual(
            candidates[1].values,
            {"normal": 10, "veteran": 20},
        )


class CliTest(unittest.TestCase):
    def test_writes_a_json_report(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "Nexus.log"
            out = Path(directory) / "report.json"
            log.write_text(
                "\n".join(
                    [
                        *probe_block(character_words=[0, 1]),
                        *probe_block(character_words=[0, 5]),
                    ]
                ),
                encoding="utf-8",
            )

            exit_code = main(
                [
                    "--log",
                    str(log),
                    "--labels",
                    "normal,champion",
                    "--out",
                    str(out),
                ]
            )

            self.assertEqual(exit_code, 0)
            report = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(report["format"], "reffect-target-classification-diff/v1")
            self.assertEqual(report["candidates"][0]["offset"], 0x304)

    def test_reports_a_label_count_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "Nexus.log"
            log.write_text("\n".join(probe_block()), encoding="utf-8")

            exit_code = main(["--log", str(log), "--labels", "normal,champion"])

            self.assertEqual(exit_code, 1)

    def test_labels_tagged_in_game_need_no_labels_argument(self) -> None:
        def tagged(sequence: int, label: str, rank: int) -> list[str]:
            block = probe_block(character_words=[0, rank])
            header = block[0].replace(
                "[reffect-target-classify] ",
                f"[reffect-target-classify] seq={sequence} ",
            )
            return [
                header,
                *block[1:],
                f"[reffect-target-classify] label={label} seq={sequence}",
            ]

        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "Nexus.log"
            log.write_text(
                "\n".join(
                    [
                        *tagged(1, "normal", 1),
                        *tagged(2, "normal", 1),
                        *tagged(3, "champion", 5),
                        *tagged(4, "champion", 5),
                    ]
                ),
                encoding="utf-8",
            )

            exit_code = main(["--log", str(log)])

            self.assertEqual(exit_code, 0)

    def test_untagged_samples_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "Nexus.log"
            log.write_text(
                "\n".join(
                    [
                        *probe_block(character_words=[0, 1]),
                        "[reffect-target-classify] label=normal",
                        *probe_block(character_words=[0, 5]),
                    ]
                ),
                encoding="utf-8",
            )

            # Only one sample is tagged, which is not enough to compare.
            exit_code = main(["--log", str(log)])

            self.assertEqual(exit_code, 1)

    def test_reports_a_log_without_probe_lines(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "Nexus.log"
            log.write_text("nothing to see", encoding="utf-8")

            exit_code = main(["--log", str(log), "--labels", "normal"])

            self.assertEqual(exit_code, 1)


if __name__ == "__main__":
    unittest.main()

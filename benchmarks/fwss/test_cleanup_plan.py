"""Behavioral boundaries of the permissionless cleanup planner."""

import io
import json
import tempfile
import unittest
from pathlib import Path

from cleanup_plan import Batches, build, data_slot, mapping_slot, piece_operations, slot_hex


class CleanupPlanTests(unittest.TestCase):
    def test_detached_32_byte_key_and_four_word_value_remain_addressable(self):
        dataset, piece = (1 << 40) - 1, (1 << 40) - 1
        key = bytes(range(32))
        key_head = data_slot(mapping_slot(piece, mapping_slot(dataset, 14)))
        # Include a zero payload word: the run still must address its fourth word.
        from eth_hash.auto import keccak
        value_head = int.from_bytes(keccak(key + mapping_slot(piece, mapping_slot(dataset, 13)).to_bytes(32, "big")), "big")
        slots = [mapping_slot(piece, mapping_slot(dataset, 14)), key_head, data_slot(key_head),
                 value_head, data_slot(value_head), data_slot(value_head) + 3]
        row = {"dataSetId": dataset, "pieceId": piece,
               "keys": [{"keyHex": key.hex(), "valueHex": (b"x" * 128).hex()}],
               "slots": [{"slot": slot_hex(s), "value": "0x01"} for s in slots]}
        ops = piece_operations(row)
        by_kind = {packed[0]: (packed, targets) for _, packed, targets in ops}
        self.assertEqual(set(by_kind), {0, 1, 2, 3, 5})
        self.assertEqual(by_kind[1][0][11], 4)
        self.assertEqual(by_kind[1][1], [data_slot(value_head) + i for i in range(4)])
        self.assertEqual(by_kind[5][1], [data_slot(key_head)])
        self.assertTrue(all(packed[1:6] == b"\xff" * 5 and packed[6:11] == b"\xff" * 5
                            for _, packed, _ in ops))
        row["slots"].append({"slot": slot_hex(123), "value": "0x01"})
        with self.assertRaisesRegex(ValueError, "unaddressable"):
            piece_operations(row)

    def test_live_queues_precede_deleted_queues_and_final_length_check(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inventory = root / "inventory"
            inventory.mkdir()
            (inventory / "occupied-slots.json").write_text(json.dumps({"address": "0x" + "12" * 20}))
            (inventory / "summary.json").write_text(json.dumps({"piecesWithMetadata": 0, "keys": 0, "nonzeroWords": 0}))
            (inventory / "pieces.jsonl").write_text("")
            queues = [{"dataSetId": dataset, "length": length, "live": live,
                       "lengthSlot": slot_hex(mapping_slot(dataset, 22)),
                       "dataStart": slot_hex(data_slot(mapping_slot(dataset, 22)))}
                      for dataset, length, live in ((1, 1, False), (2, 5, True), (3, 0, True))]
            (inventory / "queues.jsonl").write_text("".join(json.dumps(q) + "\n" for q in queues))
            build(inventory, root / "plan", max_operations=1, max_writes=3, queue_pops=2)
            batches = [json.loads(line) for line in (root / "plan/batches.jsonl").read_text().splitlines()]
            self.assertEqual([b["phase"] for b in batches], ["live-queues"] * 3 + ["deleted-queues"])
            head = queues[1]["lengthSlot"]
            self.assertNotIn(head, batches[0]["slots"])
            self.assertNotIn(head, batches[1]["slots"])
            self.assertIn(head, batches[2]["slots"])
            self.assertEqual(set().union(*(set(b["slots"]) for b in batches[:3])),
                             {head, *(slot_hex(int(queues[1]["dataStart"], 16) + i) for i in range(5))})

    def test_locality_lower_bound_is_attainable_for_prefix_grouped_writes(self):
        batches = Batches(io.BytesIO(), max_operations=2, max_writes=2, max_bytes=100)
        for prefix in (0, 0, 1, 1):
            batches.add("metadata", b"\x02", [prefix << 251])
        batches.flush()
        self.assertEqual(batches.locality_bound()["5"],
                         {"lowerBound": 2, "plannedTouches": 2, "ratio": 1.0})


if __name__ == "__main__":
    unittest.main()

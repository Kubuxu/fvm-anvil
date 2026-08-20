#!/usr/bin/env python3
"""Deterministic imported-state setup and FWSS lifecycle fixtures."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from typing import Any

from chain import Chain, Submission, quantity

OWNABLE_SLOT = "0x9016d09d72d40fdae2fd8ceac6b6234c7706214fd39c1cd1e609a0528c199300"
PROVIDER_ONE_SLOT = "0xcc69885fda6bcc1a4ace058b4a62bf5e179ea78fd58a1ccd71c22cc9b688792f"
CLEANUP_DEPOSIT = 100_000_000_000_000_000
ZERO_LEAF = "0x" + "00" * 32
ZERO_TREE_ROOT = "1f7ac9595510e09ea41c460b176430bb322cd6fb412ec57cb17d989a4310372f"
ZERO_TREE_PROOF = (
    "0x" + "00" * 32,
    "0xf5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb0b",
    "0x3731bb99ac689f66eef5973e4a94da188f4ddcae580724fc6f3fd60dfd488333",
    "0x642a607ef886b004bf2c1978463ae1d4693ac0f410eb2d1b7a47fe205e5e750f",
    "0x57a2381a28652bf47f6bef7aca679be4aede5871ab5cf3eb2c08114488cb8526",
)
VALID_PIECE_CID = "0x01559120220005" + ZERO_TREE_ROOT
DATA_SET_VIEW_SIGNATURE = (
    "getDataSet(uint256)((uint256,uint256,uint256,address,address,address,uint256,uint256,"
    "uint256,uint256,uint96,uint96,uint256))"
)


def address_word(address: str) -> str:
    return "0x" + f"{int(address, 16):064x}"


def json_array(values: list[Any]) -> str:
    return json.dumps(values, separators=(",", ":"))


@dataclass
class Dataset:
    data_set_id: int
    client_data_set_id: int
    next_nonce: int
    piece_count: int = 0

    def nonce(self) -> int:
        value = self.next_nonce
        self.next_nonce += 1
        return value


class Fixtures:
    def __init__(self, chain: Chain, suite: dict[str, Any]) -> None:
        self.chain = chain
        self.suite = suite
        self.contracts = suite["contracts"]
        self.case_seed = 1
        self.dataset_sequence = 0
        self.chain_id = quantity(chain.rpc("eth_chainId"))
        self.payee = ""

    def begin_case(self, comparison_key: str) -> None:
        digest = hashlib.sha256(comparison_key.encode()).digest()
        self.case_seed = int.from_bytes(digest[:8], "big") | 1
        self.dataset_sequence = 0

    def patch_and_fund(self) -> dict[str, Any]:
        sender_word = address_word(self.chain.sender)
        self.chain.set_storage(self.contracts["pdp"], OWNABLE_SLOT, sender_word)
        self.chain.set_storage(self.contracts["fwss"], OWNABLE_SLOT, sender_word)
        self.chain.set_storage(
            self.contracts["registry"], PROVIDER_ONE_SLOT, sender_word
        )
        reverse_slot = self.chain.cast("index", "address", self.chain.sender, "3")
        self.chain.set_storage(
            self.contracts["registry"], reverse_slot, address_word("0x1")
        )

        funding = self.suite["funding"]
        self.chain.rpc(
            "anvil_setBalance", self.chain.sender, hex(int(funding["filAtto"]))
        )
        self.chain.rpc(
            "anvil_setERC20Balance",
            self.chain.sender,
            self.contracts["usdfc"],
            hex(int(funding["usdfc"])),
        )
        self.chain.rpc(
            "anvil_setERC20Allowance",
            self.chain.sender,
            self.contracts["payments"],
            self.contracts["usdfc"],
            hex(int(funding["usdfc"])),
        )
        deposit = self.chain.send(
            to=self.contracts["payments"],
            signature="deposit(address,address,uint256)",
            arguments=[self.contracts["usdfc"], self.chain.sender, funding["deposit"]],
        )
        approval = self.chain.send(
            to=self.contracts["payments"],
            signature="setOperatorApproval(address,address,bool,uint256,uint256,uint256)",
            arguments=[
                self.contracts["usdfc"],
                self.contracts["fwss"],
                "true",
                funding["rateAllowance"],
                funding["lockupAllowance"],
                str(funding["maxLockupPeriod"]),
            ],
        )
        owner = self.chain.call(self.contracts["fwss"], "owner()(address)").strip()
        if owner.lower() != self.chain.sender.lower():
            raise RuntimeError(f"FWSS owner is {owner}, expected {self.chain.sender}")
        provider_id = self.chain.call_uint(
            self.contracts["registry"],
            "getProviderIdByAddress(address)(uint256)",
            self.chain.sender,
        )
        if provider_id != 1:
            raise RuntimeError(
                f"benchmark sender has provider ID {provider_id}, expected 1"
            )
        self.payee = self.chain.call(
            self.contracts["registry"], "getProviderPayee(uint256)(address)", "1"
        ).strip()
        return {
            "depositReceipt": deposit,
            "approvalReceipt": approval,
            "providerId": provider_id,
        }

    def _domain(self) -> dict[str, Any]:
        return {
            "name": "FilecoinWarmStorageService",
            "version": "1",
            "chainId": self.chain_id,
            "verifyingContract": self.contracts["fwss"],
        }

    def _next_client_id(self) -> int:
        self.dataset_sequence += 1
        return ((self.case_seed & ((1 << 48) - 1)) << 8) + self.dataset_sequence

    def create_dataset_submission(
        self, metadata_profile: str = "none"
    ) -> tuple[Dataset, Submission]:
        client_id = self._next_client_id()
        metadata = (
            []
            if metadata_profile == "none"
            else [{"key": "label", "value": "benchmark"}]
        )
        typed = {
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"},
                ],
                "MetadataEntry": [
                    {"name": "key", "type": "string"},
                    {"name": "value", "type": "string"},
                ],
                "CreateDataSet": [
                    {"name": "clientDataSetId", "type": "uint256"},
                    {"name": "payee", "type": "address"},
                    {"name": "metadata", "type": "MetadataEntry[]"},
                ],
            },
            "primaryType": "CreateDataSet",
            "domain": self._domain(),
            "message": {
                "clientDataSetId": client_id,
                "payee": self.payee,
                "metadata": metadata,
            },
        }
        signature = self.chain.sign_typed_data(typed)
        keys = [entry["key"] for entry in metadata]
        values = [entry["value"] for entry in metadata]
        extra = self.chain.abi_encode(
            "f(address,uint256,string[],string[],bytes)",
            self.chain.sender,
            str(client_id),
            json_array(keys),
            json_array(values),
            signature,
        )
        data_set_id = self.chain.call_uint(
            self.contracts["pdp"], "getNextDataSetId()(uint64)"
        )
        dataset = Dataset(data_set_id, client_id, client_id + 1)
        submission = self.chain.submit(
            to=self.contracts["pdp"],
            signature="createDataSet(address,bytes)",
            arguments=[self.contracts["fwss"], extra],
            value=CLEANUP_DEPOSIT,
        )
        return dataset, submission

    def assert_dataset_created(self, dataset: Dataset) -> None:
        observed = self.chain.call_uint(
            self.contracts["pdp"], "getNextDataSetId()(uint64)"
        )
        if observed != dataset.data_set_id + 1:
            raise RuntimeError(
                f"dataset counter advanced to {observed}, "
                f"expected {dataset.data_set_id + 1}"
            )

    def create_dataset(
        self, metadata_profile: str = "none"
    ) -> tuple[Dataset, dict[str, Any]]:
        dataset, submission = self.create_dataset_submission(metadata_profile)
        if submission.rejection is not None or submission.receipt is None:
            raise RuntimeError(
                f"setup createDataSet was rejected: {submission.rejection}"
            )
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                f"setup createDataSet reverted: {json.dumps(submission.receipt)}"
            )
        self.assert_dataset_created(dataset)
        return dataset, submission.receipt

    @staticmethod
    def pieces(count: int) -> str:
        return "[" + ",".join(f"({VALID_PIECE_CID})" for _ in range(count)) + "]"

    @staticmethod
    def proofs(count: int) -> str:
        siblings = "[" + ",".join(ZERO_TREE_PROOF) + "]"
        return "[" + ",".join(f"({ZERO_LEAF},{siblings})" for _ in range(count)) + "]"

    def add_pieces_submission(
        self,
        dataset: Dataset,
        count: int,
        metadata_profile: str,
        *,
        compact_empty_metadata: bool = False,
    ):
        nonce = dataset.nonce()
        metadata_entries = (
            []
            if metadata_profile == "none"
            else [{"key": "kind", "value": "benchmark"}]
        )
        typed = {
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"},
                ],
                "MetadataEntry": [
                    {"name": "key", "type": "string"},
                    {"name": "value", "type": "string"},
                ],
                "Cid": [{"name": "data", "type": "bytes"}],
                "PieceMetadata": [
                    {"name": "pieceIndex", "type": "uint256"},
                    {"name": "metadata", "type": "MetadataEntry[]"},
                ],
                "AddPieces": [
                    {"name": "clientDataSetId", "type": "uint256"},
                    {"name": "nonce", "type": "uint256"},
                    {"name": "pieceData", "type": "Cid[]"},
                    {"name": "pieceMetadata", "type": "PieceMetadata[]"},
                ],
            },
            "primaryType": "AddPieces",
            "domain": self._domain(),
            "message": {
                "clientDataSetId": dataset.client_data_set_id,
                "nonce": nonce,
                "pieceData": [{"data": VALID_PIECE_CID} for _ in range(count)],
                "pieceMetadata": (
                    []
                    if compact_empty_metadata and metadata_profile == "none"
                    else [
                        {"pieceIndex": index, "metadata": metadata_entries}
                        for index in range(count)
                    ]
                ),
            },
        }
        signature = self.chain.sign_typed_data(typed)
        keys = (
            []
            if compact_empty_metadata and metadata_profile == "none"
            else [[entry["key"] for entry in metadata_entries] for _ in range(count)]
        )
        values = (
            []
            if compact_empty_metadata and metadata_profile == "none"
            else [[entry["value"] for entry in metadata_entries] for _ in range(count)]
        )
        extra = self.chain.abi_encode(
            "f(uint256,string[][],string[][],bytes)",
            str(nonce),
            json_array(keys),
            json_array(values),
            signature,
        )
        return self.chain.submit(
            to=self.contracts["pdp"],
            signature="addPieces(uint256,address,(bytes)[],bytes)",
            arguments=[
                str(dataset.data_set_id),
                "0x0000000000000000000000000000000000000000",
                self.pieces(count),
                extra,
            ],
        )

    def add_pieces(
        self,
        dataset: Dataset,
        count: int,
        metadata_profile: str,
        *,
        compact_empty_metadata: bool = False,
    ) -> dict[str, Any]:
        submission = self.add_pieces_submission(
            dataset,
            count,
            metadata_profile,
            compact_empty_metadata=compact_empty_metadata,
        )
        if submission.rejection is not None or submission.receipt is None:
            raise RuntimeError(f"setup addPieces was rejected: {submission.rejection}")
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                f"setup addPieces reverted: {json.dumps(submission.receipt)}"
            )
        dataset.piece_count += count
        return submission.receipt

    def populate(
        self,
        dataset: Dataset,
        target: int,
        metadata_profile: str = "one",
        *,
        compact_empty_metadata: bool = False,
    ) -> list[dict[str, Any]]:
        receipts = []
        while dataset.piece_count < target:
            count = min(32, target - dataset.piece_count)
            receipts.append(
                self.add_pieces(
                    dataset,
                    count,
                    metadata_profile,
                    compact_empty_metadata=compact_empty_metadata,
                )
            )
        return receipts

    def schedule_submission(self, dataset: Dataset, piece_ids: list[int]):
        typed = {
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"},
                ],
                "SchedulePieceRemovals": [
                    {"name": "clientDataSetId", "type": "uint256"},
                    {"name": "pieceIds", "type": "uint256[]"},
                ],
            },
            "primaryType": "SchedulePieceRemovals",
            "domain": self._domain(),
            "message": {
                "clientDataSetId": dataset.client_data_set_id,
                "pieceIds": piece_ids,
            },
        }
        signature = self.chain.sign_typed_data(typed)
        extra = self.chain.abi_encode("f(bytes)", signature)
        return self.chain.submit(
            to=self.contracts["pdp"],
            signature="schedulePieceDeletions(uint256,uint256[],bytes)",
            arguments=[str(dataset.data_set_id), json_array(piece_ids), extra],
        )

    def schedule(self, dataset: Dataset, piece_ids: list[int]) -> dict[str, Any]:
        submission = self.schedule_submission(dataset, piece_ids)
        if submission.rejection is not None or submission.receipt is None:
            raise RuntimeError(f"setup schedule was rejected: {submission.rejection}")
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                f"setup schedule reverted: {json.dumps(submission.receipt)}"
            )
        return submission.receipt

    def process_deletions_submission(self, dataset: Dataset, removal_count: int):
        return self.chain.submit(
            to=self.contracts["pdp"],
            signature="processPieceDeletions(uint256,uint256)",
            arguments=[str(dataset.data_set_id), str(removal_count)],
        )

    def process_deletions(self, dataset: Dataset, removal_count: int) -> dict[str, Any]:
        submission = self.process_deletions_submission(dataset, removal_count)
        if submission.rejection is not None or submission.receipt is None:
            raise RuntimeError(
                f"setup processPieceDeletions was rejected: {submission.rejection}"
            )
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                "setup processPieceDeletions reverted: "
                f"{json.dumps(submission.receipt)}"
            )
        return submission.receipt

    def next_challenge_epoch(self, dataset: Dataset) -> int:
        del dataset
        config = self.chain.call(
            self.contracts["view"],
            "getPDPConfig()(uint64,uint256,uint256,uint256)",
        ).splitlines()
        if len(config) != 4:
            raise RuntimeError(f"unexpected getPDPConfig response: {config}")
        # The transaction executes in the following block, shifting the first window by one.
        return int(config[3].split()[0]) + 1

    def next_period_submission(self, dataset: Dataset):
        challenge = self.next_challenge_epoch(dataset)
        return challenge, self.chain.submit(
            to=self.contracts["pdp"],
            signature="nextProvingPeriod(uint256,uint256,bytes)",
            arguments=[str(dataset.data_set_id), str(challenge), "0x"],
        )

    def activate(self, dataset: Dataset) -> tuple[int, dict[str, Any]]:
        challenge, submission = self.next_period_submission(dataset)
        if submission.rejection is not None or submission.receipt is None:
            raise RuntimeError(f"activation rejected: {submission.rejection}")
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(f"activation reverted: {json.dumps(submission.receipt)}")
        return challenge, submission.receipt

    def prove_submission(self, dataset: Dataset, challenges: int):
        fee = self.chain.call_uint(
            self.contracts["pdp"],
            "calculateProofFee(uint256)(uint256)",
            str(dataset.data_set_id),
        )
        return fee, self.chain.submit(
            to=self.contracts["pdp"],
            signature="provePossession(uint256,(bytes32,bytes32[])[])",
            arguments=[str(dataset.data_set_id), self.proofs(challenges)],
            value=fee,
        )

    def data_set_payment_rail(self, dataset: Dataset) -> tuple[int, int]:
        result = self.chain.call_json(
            self.contracts["view"], DATA_SET_VIEW_SIGNATURE, str(dataset.data_set_id)
        )
        values = result[0] if isinstance(result, list) and len(result) == 1 else result
        if isinstance(values, dict):
            return quantity(values["pdpRailId"]), quantity(values["pdpEndEpoch"])
        return quantity(values[0]), quantity(values[8])

    def settle_before_activation(self, dataset: Dataset) -> dict[str, Any]:
        rail_id, _ = self.data_set_payment_rail(dataset)
        return self.chain.send(
            to=self.contracts["payments"],
            signature="settleRail(uint256,uint256)",
            arguments=[str(rail_id), str(self.chain.block_number())],
        )

    def terminate_submission(self, dataset: Dataset):
        return self.chain.submit(
            to=self.contracts["fwss"],
            signature="terminateService(uint256)",
            arguments=[str(dataset.data_set_id)],
        )

    def terminate_and_settle(self, dataset: Dataset) -> dict[str, Any]:
        termination = self.terminate_submission(dataset)
        if termination.rejection is not None or termination.receipt is None:
            raise RuntimeError(f"termination rejected: {termination.rejection}")
        if quantity(termination.receipt["status"]) != 1:
            raise RuntimeError(
                f"termination reverted: {json.dumps(termination.receipt)}"
            )
        rail_id, end_epoch = self.data_set_payment_rail(dataset)
        config = self.chain.call(
            self.contracts["view"],
            "getPDPConfig()(uint64,uint256,uint256,uint256)",
        ).splitlines()
        if len(config) != 4:
            raise RuntimeError(f"unexpected getPDPConfig response: {config}")
        max_proving_period = int(config[0].split()[0])
        # The final partial proving period remains open until its deadline passes.
        self.chain.mine_to(end_epoch + max_proving_period + 1)
        settlement = self.chain.send(
            to=self.contracts["payments"],
            signature="settleRail(uint256,uint256)",
            arguments=[str(rail_id), str(end_epoch)],
        )
        return {
            "terminationReceipt": termination.receipt,
            "settlementReceipt": settlement,
        }

    def delete_submission(self, dataset: Dataset):
        return self.chain.submit(
            to=self.contracts["pdp"],
            signature="deleteDataSet(uint256,bytes)",
            arguments=[str(dataset.data_set_id), "0x"],
        )

    def cleanup_submission(self, dataset: Dataset, batch_size: int):
        return self.chain.submit(
            to=self.contracts["pdp"],
            signature="cleanupPieces(uint256,uint256)",
            arguments=[str(dataset.data_set_id), str(batch_size)],
        )

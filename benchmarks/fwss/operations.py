#!/usr/bin/env python3
"""FWSS benchmark case generation, fixtures, execution, and outcome checks."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Callable

from chain import Chain, Submission, quantity
from fixtures import Dataset, Fixtures
from traces import summarize_trace

SCHEDULE_REMOVALS_BATCH_LIMIT = 2_000


def capped_batch_size(operation: str, size: int) -> int:
    if operation in ("schedulePieceDeletions", "nextProvingPeriod"):
        return min(size, SCHEDULE_REMOVALS_BATCH_LIMIT)
    return size


class SetupUnreachable(RuntimeError):
    pass


@dataclass
class PreparedCase:
    action: Callable[[], tuple[Submission, dict[str, Any]]]
    postcondition: Callable[[], None]
    setup: dict[str, Any]


def comparison_key(case: dict[str, Any]) -> str:
    operation = case["operation"]
    parameters = case.get("parameters", {})
    suffix = "/".join(f"{key}-{parameters[key]}" for key in sorted(parameters))
    return operation if not suffix else f"{operation}/{suffix}"


def fixed_cases(suite: dict[str, Any]) -> list[dict[str, Any]]:
    config = suite["cases"]
    cases: list[dict[str, Any]] = [
        {"operation": "createDataSet", "parameters": {"metadata": profile}}
        for profile in ("none", "one")
    ]
    for batch_size in config["batchSizes"]:
        removal_batch_size = capped_batch_size("schedulePieceDeletions", batch_size)
        for metadata in config["metadataProfiles"]:
            for starting_count in config["startingPieceCounts"]:
                cases.append(
                    {
                        "operation": "addPieces",
                        "parameters": {
                            "batchSize": batch_size,
                            "metadata": metadata,
                            "startingPieceCount": starting_count,
                        },
                    }
                )
        cases.append(
            {
                "operation": "schedulePieceDeletions",
                "parameters": {"batchSize": removal_batch_size},
            }
        )
        cases.append(
            {
                "operation": "nextProvingPeriod",
                "parameters": {"removalCount": removal_batch_size},
            }
        )
        cases.append(
            {
                "operation": "schedulePieceDeletions",
                "parameters": {"batchSize": removal_batch_size, "metadata": "none"},
            }
        )
        cases.append(
            {
                "operation": "nextProvingPeriod",
                "parameters": {"removalCount": removal_batch_size, "metadata": "none"},
            }
        )
        cases.append(
            {"operation": "cleanupPieces", "parameters": {"batchSize": batch_size}}
        )
    cases.extend(
        [
            {
                "operation": "provePossession",
                "parameters": {"challengeCount": config["proofChallenges"]},
            },
            {"operation": "terminateService", "parameters": {"state": "active"}},
            {"operation": "deleteDataSet", "parameters": {"state": "empty"}},
            {
                "operation": "deleteDataSet",
                "parameters": {
                    "state": "populated",
                    "pieceCount": config["populatedDeletePieces"],
                },
            },
        ]
    )
    return cases


def _successful(submission: Submission, purpose: str) -> dict[str, Any]:
    if submission.rejection is not None or submission.receipt is None:
        raise SetupUnreachable(
            f"{purpose} rejected: {json.dumps(submission.rejection)}"
        )
    if quantity(submission.receipt["status"]) != 1:
        raise SetupUnreachable(f"{purpose} reverted: {json.dumps(submission.receipt)}")
    return submission.receipt


def prepare_case(
    fixtures: Fixtures,
    case: dict[str, Any],
    *,
    compact_empty_metadata: bool = False,
    compact_setup_metadata: bool = False,
) -> PreparedCase:
    operation = case["operation"]
    parameters = case.get("parameters", {})
    pdp = fixtures.contracts["pdp"]

    if operation == "createDataSet":
        profile = parameters["metadata"]
        created: dict[str, Dataset] = {}

        def action() -> tuple[Submission, dict[str, Any]]:
            dataset, submission = fixtures.create_dataset_submission(profile)
            created["dataset"] = dataset
            return submission, {"dataSetId": dataset.data_set_id}

        def postcondition() -> None:
            dataset = created.get("dataset")
            if dataset is None:
                raise RuntimeError("createDataSet did not produce a dataset")
            fixtures.assert_dataset_created(dataset)

        return PreparedCase(action, postcondition, {})

    dataset, create_receipt = fixtures.create_dataset()
    setup: dict[str, Any] = {
        "dataSetId": dataset.data_set_id,
        "createTransactionHash": create_receipt["transactionHash"],
    }

    if operation == "addPieces":
        starting = int(parameters["startingPieceCount"])
        metadata = parameters["metadata"]
        fixtures.populate(
            dataset,
            starting,
            metadata,
            compact_empty_metadata=compact_setup_metadata,
        )
        batch_size = int(parameters["batchSize"])

        def action() -> tuple[Submission, dict[str, Any]]:
            return fixtures.add_pieces_submission(
                dataset,
                batch_size,
                metadata,
                compact_empty_metadata=compact_empty_metadata,
            ), {
                "dataSetId": dataset.data_set_id,
                "startingPieceCount": starting,
            }

        def postcondition() -> None:
            observed = fixtures.chain.call_uint(
                pdp, "getNextPieceId(uint256)(uint256)", str(dataset.data_set_id)
            )
            if observed != starting + batch_size:
                raise RuntimeError(
                    f"dataset has {observed} pieces, expected {starting + batch_size}"
                )

        return PreparedCase(action, postcondition, setup)

    if operation == "schedulePieceDeletions":
        batch_size = int(parameters["batchSize"])
        fixtures.populate(
            dataset,
            batch_size + 1,
            parameters.get("metadata", "one"),
            compact_empty_metadata=compact_setup_metadata,
        )
        piece_ids = list(range(batch_size))
        return PreparedCase(
            lambda: (
                fixtures.schedule_submission(dataset, piece_ids),
                {"dataSetId": dataset.data_set_id, "pieceIds": piece_ids},
            ),
            lambda: None,
            setup,
        )

    if operation == "nextProvingPeriod":
        removal_count = int(parameters["removalCount"])
        fixtures.populate(
            dataset,
            removal_count + 1,
            parameters.get("metadata", "one"),
            compact_empty_metadata=compact_setup_metadata,
        )
        piece_ids = list(range(removal_count))
        try:
            setup_receipt = fixtures.schedule(dataset, piece_ids)
        except RuntimeError as error:
            raise SetupUnreachable(str(error)) from error
        setup["scheduleTransactionHash"] = setup_receipt["transactionHash"]
        challenge_holder: dict[str, int] = {}

        def action() -> tuple[Submission, dict[str, Any]]:
            challenge, submission = fixtures.next_period_submission(dataset)
            challenge_holder["challenge"] = challenge
            return submission, {
                "dataSetId": dataset.data_set_id,
                "challengeEpoch": challenge,
                "removedPieceCount": removal_count,
            }

        def postcondition() -> None:
            if removal_count:
                leaf_count = fixtures.chain.call_uint(
                    pdp,
                    "getPieceLeafCount(uint256,uint256)(uint256)",
                    str(dataset.data_set_id),
                    "0",
                )
                if leaf_count != 0:
                    raise RuntimeError(
                        "nextProvingPeriod did not apply the first removal"
                    )

        return PreparedCase(action, postcondition, setup)

    if operation == "provePossession":
        challenges = int(parameters["challengeCount"])
        fixtures.populate(
            dataset,
            128,
            "one",
            compact_empty_metadata=compact_setup_metadata,
        )
        challenge_epoch, activation = fixtures.activate(dataset)
        fixtures.chain.mine_to(challenge_epoch)
        setup["activationTransactionHash"] = activation["transactionHash"]

        def action() -> tuple[Submission, dict[str, Any]]:
            fee, submission = fixtures.prove_submission(dataset, challenges)
            return submission, {
                "dataSetId": dataset.data_set_id,
                "proofFee": fee,
                "challengeCount": challenges,
            }

        return PreparedCase(action, lambda: None, setup)

    if operation == "terminateService":
        fixtures.populate(
            dataset,
            128,
            "one",
            compact_empty_metadata=compact_setup_metadata,
        )
        _, activation = fixtures.activate(dataset)
        setup["activationTransactionHash"] = activation["transactionHash"]
        return PreparedCase(
            lambda: (
                fixtures.terminate_submission(dataset),
                {"dataSetId": dataset.data_set_id},
            ),
            lambda: None,
            setup,
        )

    if operation == "deleteDataSet":
        state = parameters["state"]
        if state == "populated":
            piece_count = int(parameters["pieceCount"])
            fixtures.populate(
                dataset,
                piece_count,
                "one",
                compact_empty_metadata=compact_setup_metadata,
            )
            pre_activation_settlement = fixtures.settle_before_activation(dataset)
            setup["preActivationSettlementTransactionHash"] = pre_activation_settlement[
                "transactionHash"
            ]
            _, activation = fixtures.activate(dataset)
            setup["activationTransactionHash"] = activation["transactionHash"]
            try:
                settlement = fixtures.terminate_and_settle(dataset)
            except RuntimeError as error:
                raise SetupUnreachable(str(error)) from error
            setup["terminationTransactionHash"] = settlement["terminationReceipt"][
                "transactionHash"
            ]
            setup["settlementTransactionHash"] = settlement["settlementReceipt"][
                "transactionHash"
            ]

        def postcondition() -> None:
            live = fixtures.chain.call(
                pdp, "dataSetLive(uint256)(bool)", str(dataset.data_set_id)
            ).strip()
            if live != "false":
                raise RuntimeError("deleteDataSet left the dataset live")

        return PreparedCase(
            lambda: (
                fixtures.delete_submission(dataset),
                {"dataSetId": dataset.data_set_id, "state": state},
            ),
            postcondition,
            setup,
        )

    if operation == "cleanupPieces":
        batch_size = int(parameters["batchSize"])
        piece_count = max(
            batch_size, int(fixtures.suite["cases"]["populatedDeletePieces"])
        )
        fixtures.populate(
            dataset,
            piece_count,
            "one",
            compact_empty_metadata=compact_setup_metadata,
        )
        pre_activation_settlement = fixtures.settle_before_activation(dataset)
        setup["preActivationSettlementTransactionHash"] = pre_activation_settlement[
            "transactionHash"
        ]
        _, activation = fixtures.activate(dataset)
        setup["activationTransactionHash"] = activation["transactionHash"]
        try:
            settlement = fixtures.terminate_and_settle(dataset)
            deletion = _successful(
                fixtures.delete_submission(dataset), "deleteDataSet setup"
            )
        except RuntimeError as error:
            raise SetupUnreachable(str(error)) from error
        setup.update(
            {
                "pieceCount": piece_count,
                "terminationTransactionHash": settlement["terminationReceipt"][
                    "transactionHash"
                ],
                "settlementTransactionHash": settlement["settlementReceipt"][
                    "transactionHash"
                ],
                "deleteTransactionHash": deletion["transactionHash"],
            }
        )
        return PreparedCase(
            lambda: (
                fixtures.cleanup_submission(dataset, batch_size),
                {"dataSetId": dataset.data_set_id, "startingPieceCount": piece_count},
            ),
            lambda: None,
            setup,
        )

    raise ValueError(f"unsupported operation {operation}")


def measure_prepared(
    chain: Chain,
    prepared: PreparedCase,
    case: dict[str, Any],
    labels: dict[str, str],
) -> dict[str, Any]:
    submission, runtime = prepared.action()
    result: dict[str, Any] = {
        "operation": case["operation"],
        "parameters": case.get("parameters", {}),
        "setup": prepared.setup,
        "runtime": runtime,
    }
    if submission.rejection is not None:
        result.update(
            {"status": "admission-failure", "rejection": submission.rejection}
        )
        return result
    assert submission.receipt is not None and submission.transaction_hash is not None
    receipt = submission.receipt
    trace = chain.trace(submission.transaction_hash)
    summary = summarize_trace(trace, labels)
    receipt_gas = quantity(receipt["gasUsed"])
    if summary["gasUsed"] != receipt_gas:
        raise RuntimeError(
            f"trace gas {summary['gasUsed']} differs from receipt gas {receipt_gas}"
        )
    succeeded = quantity(receipt["status"]) == 1 and quantity(trace["exitCode"]) == 0
    if succeeded:
        prepared.postcondition()
        status = "success"
    else:
        failure = str(trace.get("failureInfo") or "").lower()
        status = "out-of-gas" if "gas" in failure else "contract-revert"
    result.update(
        {
            "status": status,
            "transaction": receipt,
            "traceSummary": summary,
            "trace": trace,
        }
    )
    return result

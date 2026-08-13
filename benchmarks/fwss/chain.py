#!/usr/bin/env python3
"""Small `cast` and JSON-RPC wrapper for the FWSS benchmark."""

from __future__ import annotations

import json
import subprocess
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from eth_account import Account


class CommandError(RuntimeError):
    def __init__(
        self, args: list[str], returncode: int, stdout: str, stderr: str
    ) -> None:
        self.args_list = args
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr
        super().__init__(
            f"command failed ({returncode}): {' '.join(args[:5])}\n"
            f"stdout:\n{stdout}\nstderr:\n{stderr}"
        )


class JsonRpcError(RuntimeError):
    def __init__(self, method: str, error: Any) -> None:
        self.method = method
        self.error = error
        super().__init__(f"JSON-RPC {method} failed: {error}")


def command(args: list[str], *, cwd: Path | None = None) -> str:
    completed = subprocess.run(
        args,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        raise CommandError(
            args, completed.returncode, completed.stdout, completed.stderr
        )
    return completed.stdout.strip()


def quantity(value: str | int) -> int:
    if isinstance(value, str) and value.startswith("0x"):
        return int(value, 16)
    return int(value)


@dataclass(frozen=True)
class Submission:
    transaction_hash: str | None
    receipt: dict[str, Any] | None
    rejection: dict[str, Any] | None


class Chain:
    def __init__(self, rpc_url: str, private_key: str, gas_limit: int) -> None:
        self.rpc_url = rpc_url
        self.private_key = private_key
        self.gas_limit = gas_limit
        self.sender = self.cast("wallet", "address", "--private-key", private_key)
        self.max_fee_per_gas = quantity(self.rpc("eth_gasPrice"))
        self.priority_fee_per_gas = 1
        self.chain_id = quantity(self.rpc("eth_chainId"))

    def cast(self, *args: str, cwd: Path | None = None) -> str:
        return command(["cast", *args], cwd=cwd)

    def rpc(self, method: str, *params: str) -> Any:
        output = self.cast("rpc", "--rpc-url", self.rpc_url, method, *params)
        return json.loads(output)

    def rpc_http(self, method: str, params: list[Any]) -> Any:
        request = urllib.request.Request(
            self.rpc_url,
            data=json.dumps(
                {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
            ).encode(),
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                payload = json.loads(response.read())
        except urllib.error.URLError as error:
            raise RuntimeError(f"JSON-RPC request failed: {error}") from error
        if "error" in payload:
            raise JsonRpcError(method, payload["error"])
        return payload["result"]

    def submit_large(
        self,
        to: str,
        signature: str,
        arguments: list[str],
        value: int | None,
    ) -> str:
        with tempfile.NamedTemporaryFile("w", delete=False) as argument_file:
            argument_file.write("\n".join(arguments))
            argument_path = Path(argument_file.name)
        try:
            calldata = self.cast("calldata", signature, "--file", str(argument_path))
        finally:
            argument_path.unlink()
        transaction = {
            "type": 2,
            "chainId": self.chain_id,
            "nonce": quantity(
                self.rpc("eth_getTransactionCount", self.sender, "pending")
            ),
            "gas": self.gas_limit,
            "maxFeePerGas": self.max_fee_per_gas,
            "maxPriorityFeePerGas": self.priority_fee_per_gas,
            "to": bytes.fromhex(to.removeprefix("0x")),
            "value": value or 0,
            "data": bytes.fromhex(calldata.removeprefix("0x")),
            "accessList": [],
        }
        signed = Account.sign_transaction(transaction, self.private_key)
        raw_transaction = signed.raw_transaction.hex()
        if not raw_transaction.startswith("0x"):
            raw_transaction = "0x" + raw_transaction
        return str(self.rpc_http("eth_sendRawTransaction", [raw_transaction]))

    def call(self, to: str, signature: str, *arguments: str) -> str:
        return self.cast("call", "--rpc-url", self.rpc_url, to, signature, *arguments)

    def call_json(self, to: str, signature: str, *arguments: str) -> Any:
        output = self.cast(
            "call", "--json", "--rpc-url", self.rpc_url, to, signature, *arguments
        )
        return json.loads(output)

    def call_uint(self, to: str, signature: str, *arguments: str) -> int:
        return int(self.call(to, signature, *arguments).split()[0])

    def block_number(self) -> int:
        return int(self.cast("block-number", "--rpc-url", self.rpc_url))

    def snapshot(self) -> str:
        return str(self.rpc("anvil_snapshot"))

    def revert(self, snapshot: str) -> None:
        if self.rpc("anvil_revert", snapshot) is not True:
            raise RuntimeError(f"failed to revert snapshot {snapshot}")

    def mine(self) -> None:
        self.rpc("evm_mine")

    def mine_to(self, target: int) -> None:
        current = self.block_number()
        if current < target:
            self.rpc("anvil_mine", hex(target - current))

    def set_storage(self, address: str, slot: str, value: str) -> None:
        result = self.rpc("anvil_setStorageAt", address, slot, value)
        if result is not True:
            raise RuntimeError(f"anvil_setStorageAt returned {result!r}")

    def submit(
        self,
        *,
        to: str | None = None,
        signature: str | None = None,
        arguments: list[str] | None = None,
        value: int | None = None,
        creation_code: str | None = None,
    ) -> Submission:
        args = [
            "cast",
            "send",
            "--async",
            "--rpc-url",
            self.rpc_url,
            "--private-key",
            self.private_key,
            "--gas-limit",
            str(self.gas_limit),
            "--gas-price",
            str(self.max_fee_per_gas),
            "--priority-gas-price",
            str(self.priority_fee_per_gas),
        ]
        if value is not None:
            args.extend(["--value", str(value)])
        if creation_code is not None:
            args.extend(["--create", creation_code])
        else:
            if to is None or signature is None:
                raise ValueError("transaction requires to and signature")
            call_arguments = arguments or []
            args.extend([to, signature, *call_arguments])
        try:
            if (
                creation_code is None
                and sum(len(argument) for argument in call_arguments) > 100_000
            ):
                assert to is not None and signature is not None
                transaction_hash = self.submit_large(
                    to, signature, call_arguments, value
                )
            else:
                transaction_hash = command(args)
        except JsonRpcError as error:
            return Submission(
                transaction_hash=None,
                receipt=None,
                rejection={"rpcError": error.error},
            )
        except CommandError as error:
            return Submission(
                transaction_hash=None,
                receipt=None,
                rejection={
                    "returncode": error.returncode,
                    "stdout": error.stdout,
                    "stderr": error.stderr,
                },
            )
        try:
            receipt = json.loads(
                self.cast(
                    "receipt",
                    "--rpc-url",
                    self.rpc_url,
                    "--confirmations",
                    "1",
                    "--json",
                    transaction_hash,
                )
            )
        except CommandError as error:
            raise RuntimeError(
                f"submitted {transaction_hash} but could not fetch receipt"
            ) from error
        return Submission(
            transaction_hash=transaction_hash, receipt=receipt, rejection=None
        )

    def send(self, **kwargs: Any) -> dict[str, Any]:
        submission = self.submit(**kwargs)
        if submission.rejection is not None:
            raise RuntimeError(
                f"transaction rejected: {json.dumps(submission.rejection)}"
            )
        assert submission.receipt is not None
        if quantity(submission.receipt["status"]) != 1:
            raise RuntimeError(
                f"transaction reverted: {json.dumps(submission.receipt)}"
            )
        return submission.receipt

    def deploy(self, creation_code: str) -> tuple[str, dict[str, Any]]:
        receipt = self.send(creation_code=creation_code)
        address = receipt.get("contractAddress")
        if not address:
            raise RuntimeError(
                f"deployment has no contract address: {json.dumps(receipt)}"
            )
        return address, receipt

    def trace(self, transaction_hash: str) -> dict[str, Any]:
        trace = self.rpc("filecoin_getGasTrace", transaction_hash)
        if not isinstance(trace, dict):
            raise RuntimeError(f"missing Filecoin gas trace for {transaction_hash}")
        return trace

    def abi_encode(self, signature: str, *arguments: str) -> str:
        return self.cast("abi-encode", signature, *arguments)

    def calldata(self, signature: str, *arguments: str) -> str:
        return self.cast("calldata", signature, *arguments)

    def sign_typed_data(self, data: dict[str, Any]) -> str:
        encoded = json.dumps(data, separators=(",", ":"))
        if len(encoded) <= 100_000:
            return self.cast(
                "wallet",
                "sign",
                "--private-key",
                self.private_key,
                "--data",
                encoded,
            )
        signature = Account.sign_typed_data(
            self.private_key, full_message=data
        ).signature.hex()
        return signature if signature.startswith("0x") else "0x" + signature

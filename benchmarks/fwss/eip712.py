#!/usr/bin/env python3
"""EIP-712 digests and signatures for FWSS v1.3.0, computed through `cast`.

Typehash strings and struct layouts are pinned to FilOzone/filecoin-services
commit c2ff7f8ac574f62e5d9f9ace2cf2efbb64b0f3a4
(service_contracts/src/lib/SignatureVerificationLib.sol). Everything shells out
to `cast keccak` / `cast abi-encode` / `cast wallet sign --no-hash` so the
harness has no Python dependencies beyond the standard library.
"""

from __future__ import annotations

import subprocess

DOMAIN_TYPE = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
DOMAIN_NAME = "FilecoinWarmStorageService"
DOMAIN_VERSION = "1"

METADATA_ENTRY_TYPE = "MetadataEntry(string key,string value)"
CREATE_DATA_SET_TYPE = (
    "CreateDataSet(uint256 clientDataSetId,address payee,MetadataEntry[] metadata)" + METADATA_ENTRY_TYPE
)
CID_TYPE = "Cid(bytes data)"
PIECE_METADATA_BASE = "PieceMetadata(uint256 pieceIndex,MetadataEntry[] metadata)"
PIECE_METADATA_TYPE = PIECE_METADATA_BASE + METADATA_ENTRY_TYPE
ADD_PIECES_TYPE = (
    "AddPieces(uint256 clientDataSetId,uint256 nonce,Cid[] pieceData,PieceMetadata[] pieceMetadata)"
    + CID_TYPE
    + METADATA_ENTRY_TYPE
    + PIECE_METADATA_BASE
)

# keccak256("") == hashMetadataEntries([]) == hash of any empty encodePacked array.
EMPTY_HASH = "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"


def command(args: list[str]) -> str:
    completed = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if completed.returncode != 0:
        rendered = " ".join(args[:4])
        raise RuntimeError(
            f"command failed ({completed.returncode}): {rendered}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed.stdout.strip()


def keccak(data: str) -> str:
    """Hash a 0x-prefixed byte string, or the UTF-8 bytes of a plain string."""
    return command(["cast", "keccak", data])


def abi_encode(signature: str, *args: str) -> str:
    return command(["cast", "abi-encode", signature, *args])


def concat_hash(words: list[str]) -> str:
    """keccak256(abi.encodePacked(bytes32[]))."""
    if not words:
        return EMPTY_HASH
    return keccak("0x" + "".join(word[2:] for word in words))


class Eip712Signer:
    """Builds FWSS operation digests for one verifying contract and chain."""

    def __init__(self, fwss_proxy: str, chain_id: int) -> None:
        self.type_hashes = {
            name: keccak(value)
            for name, value in (
                ("domain", DOMAIN_TYPE),
                ("createDataSet", CREATE_DATA_SET_TYPE),
                ("cid", CID_TYPE),
                ("pieceMetadata", PIECE_METADATA_TYPE),
                ("addPieces", ADD_PIECES_TYPE),
            )
        }
        self.domain_separator = keccak(
            abi_encode(
                "f(bytes32,bytes32,bytes32,uint256,address)",
                self.type_hashes["domain"],
                keccak(DOMAIN_NAME),
                keccak(DOMAIN_VERSION),
                str(chain_id),
                fwss_proxy,
            )
        )

    def digest(self, struct_hash: str) -> str:
        return keccak("0x1901" + self.domain_separator[2:] + struct_hash[2:])

    def create_data_set_digest(self, client_data_set_id: int, payee: str) -> str:
        """CreateDataSet with empty metadata."""
        struct_hash = keccak(
            abi_encode(
                "f(bytes32,uint256,address,bytes32)",
                self.type_hashes["createDataSet"],
                str(client_data_set_id),
                payee,
                EMPTY_HASH,
            )
        )
        return self.digest(struct_hash)

    def add_pieces_digest(self, client_data_set_id: int, nonce: int, cid_bytes: list[str]) -> str:
        """AddPieces with empty per-piece metadata; cid_bytes are 0x-prefixed CommPv2 CID bytes."""
        cid_hashes = [
            keccak(abi_encode("f(bytes32,bytes32)", self.type_hashes["cid"], keccak(cid)))
            for cid in cid_bytes
        ]
        piece_metadata_hashes = [
            keccak(abi_encode("f(bytes32,uint256,bytes32)", self.type_hashes["pieceMetadata"], str(i), EMPTY_HASH))
            for i in range(len(cid_bytes))
        ]
        struct_hash = keccak(
            abi_encode(
                "f(bytes32,uint256,uint256,bytes32,bytes32)",
                self.type_hashes["addPieces"],
                str(client_data_set_id),
                str(nonce),
                concat_hash(cid_hashes),
                concat_hash(piece_metadata_hashes),
            )
        )
        return self.digest(struct_hash)


def sign_digest(private_key: str, digest: str) -> str:
    """65-byte (r, s, v) signature over a raw 32-byte digest; v is 27 or 28."""
    return command(["cast", "wallet", "sign", "--no-hash", "--private-key", private_key, digest])

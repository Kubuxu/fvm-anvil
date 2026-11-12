// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

/// @notice Lab prototype of batched, metadata-only historical cleanup.
/// @dev List deletion may orphan values. Value deletion never consults the key list.
/// Production integration must use FWSS authorization and permanently disable metadata writes.
contract TypedMetadataCleanup {
    struct MetadataStorage {
        mapping(uint256 => mapping(uint256 => mapping(string => string))) values;
        mapping(uint256 => mapping(uint256 => string[])) keys;
    }

    struct Deletion {
        uint32 dataSetId;
        uint32 pieceId;
        uint8 target;
        bytes key;
    }

    address public immutable operator;

    error Unauthorized();
    error ListHasKey();
    error InvalidPackedBatch();
    error InvalidTarget();

    constructor(address operator_) {
        operator = operator_;
    }

    function _metadata() private pure returns (MetadataStorage storage state) {
        assembly ("memory-safe") { state.slot := 13 }
    }

    /// @notice Executes a caller-ordered batch; addresses are derived only from metadata identities.
    /// @dev Targets: 0 deletes a value, 1 deletes a list, 2 zeroes a value without changing its length.
    function cleanup(Deletion[] calldata batch) external {
        if (msg.sender != operator) revert Unauthorized();
        MetadataStorage storage state = _metadata();
        for (uint256 i; i < batch.length; ++i) {
            Deletion calldata item = batch[i];
            _apply(state, item.dataSetId, item.pieceId, item.target, item.key);
        }
    }

    function _apply(
        MetadataStorage storage state, uint32 dataSetId, uint32 pieceId, uint8 target, bytes calldata key
    ) private {
        if (target == 0) {
            delete state.values[dataSetId][pieceId][string(key)];
        } else if (target == 1) {
            if (key.length != 0) revert ListHasKey();
            delete state.keys[dataSetId][pieceId];
        } else if (target == 2) {
            uint256 length = bytes(state.values[dataSetId][pieceId][string(key)]).length;
            state.values[dataSetId][pieceId][string(key)] = new string(length);
        } else {
            revert InvalidTarget();
        }
    }
    /// @notice Packed records: uint32 dataset | uint32 piece | uint8 target | uint16 key length | key.
    /// @dev Targets match cleanup; list operations require an empty key. Integers are big-endian.
    function cleanupPacked(bytes calldata batch) external {
        if (msg.sender != operator) revert Unauthorized();
        MetadataStorage storage state = _metadata();
        uint256 cursor;
        while (cursor < batch.length) {
            if (batch.length - cursor < 11) revert InvalidPackedBatch();
            uint32 dataSetId;
            uint32 pieceId;
            uint8 target;
            uint16 keyLength;
            assembly ("memory-safe") {
                let header := calldataload(add(batch.offset, cursor))
                dataSetId := shr(224, header)
                pieceId := and(shr(192, header), 0xffffffff)
                target := and(shr(184, header), 0xff)
                keyLength := and(shr(168, header), 0xffff)
            }
            cursor += 11;
            if (keyLength > batch.length - cursor) revert InvalidPackedBatch();
            _apply(state, dataSetId, pieceId, target, batch[cursor:cursor + keyLength]);
            cursor += keyLength;
        }
    }
    function slotsMatch(bytes32[] calldata slots, bytes32[] calldata expected) external view returns (bool) {
        if (slots.length != expected.length) return false;
        for (uint256 i; i < slots.length; ++i) {
            bytes32 value;
            assembly ("memory-safe") { value := sload(calldataload(add(slots.offset, mul(i, 32)))) }
            if (value != expected[i]) return false;
        }
        return true;
    }

    function slotsAreZero(bytes32[] calldata slots) external view returns (bool) {
        for (uint256 i; i < slots.length; ++i) {
            bytes32 value;
            assembly ("memory-safe") { value := sload(calldataload(add(slots.offset, mul(i, 32)))) }
            if (value != bytes32(0)) return false;
        }
        return true;
    }
}

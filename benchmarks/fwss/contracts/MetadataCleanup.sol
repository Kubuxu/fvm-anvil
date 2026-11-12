// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

/// @notice Lab-only runtime installed with anvil_setCode at the historical FWSS proxy.
/// @dev This is not an upgrade. It only targets historical FWSS mapping roots 13 and 14.
contract MetadataCleanup {
    uint256 private constant PIECE_METADATA_SLOT = 13;

    struct MetadataStorage {
        mapping(uint256 => mapping(uint256 => mapping(string => string))) values;
        mapping(uint256 => mapping(uint256 => string[])) keys;
    }

    address public immutable operator;

    error Unauthorized(address caller);
    error InvalidRange(uint256 dataSetId, uint256 firstPieceId, uint256 endPieceId);
    error LengthMismatch();

    constructor(address operator_) {
        operator = operator_;
    }

    modifier onlyOperator() {
        if (msg.sender != operator) revert Unauthorized(msg.sender);
        _;
    }

    function _metadata() private pure returns (MetadataStorage storage state) {
        assembly ("memory-safe") { state.slot := PIECE_METADATA_SLOT }
    }

    /// @notice Deletes historical metadata in dataset/piece/key order.
    /// @dev Ranges are inclusive-exclusive. Empty pieces are deliberately scanned.
    function deleteNatural(
        uint256[] calldata dataSetIds,
        uint256[] calldata firstPieceIds,
        uint256[] calldata endPieceIds
    ) external onlyOperator {
        if (dataSetIds.length != firstPieceIds.length || dataSetIds.length != endPieceIds.length) {
            revert LengthMismatch();
        }
        MetadataStorage storage state = _metadata();
        for (uint256 range; range < dataSetIds.length; ++range) {
            uint256 firstPieceId = firstPieceIds[range];
            uint256 endPieceId = endPieceIds[range];
            if (endPieceId < firstPieceId) {
                revert InvalidRange(dataSetIds[range], firstPieceId, endPieceId);
            }
            for (uint256 pieceId = firstPieceId; pieceId < endPieceId; ++pieceId) {
                string[] storage keys = state.keys[dataSetIds[range]][pieceId];
                uint256 keyCount = keys.length;
                for (uint256 keyIndex; keyIndex < keyCount; ++keyIndex) {
                    delete state.values[dataSetIds[range]][pieceId][keys[keyIndex]];
                }
                delete state.keys[dataSetIds[range]][pieceId];
            }
        }
    }

    /// @notice Control path: clears exactly the caller-provided words in caller order.
    function deleteSlots(bytes32[] calldata slots) external onlyOperator {
        for (uint256 index; index < slots.length; ++index) {
            assembly ("memory-safe") { sstore(calldataload(add(slots.offset, mul(index, 0x20))), 0) }
        }
    }

    function metadataKeysEmpty(uint256[] calldata dataSetIds, uint256[] calldata pieceIds)
        external view returns (bool)
    {
        if (dataSetIds.length != pieceIds.length) return false;
        MetadataStorage storage state = _metadata();
        for (uint256 index; index < dataSetIds.length; ++index) {
            if (state.keys[dataSetIds[index]][pieceIds[index]].length != 0) return false;
        }
        return true;
    }

    /// @notice Exposes a root-12 dataset-metadata key-array header for preservation checks.
    function dataSetMetadataKeysSlot(uint256 dataSetId) external view returns (bytes32 slot, bytes32 value) {
        slot = keccak256(abi.encode(dataSetId, uint256(12)));
        assembly ("memory-safe") { value := sload(slot) }
    }

    function slotsAreZero(bytes32[] calldata slots) external view returns (bool) {
        for (uint256 index; index < slots.length; ++index) {
            bytes32 value;
            assembly ("memory-safe") { value := sload(calldataload(add(slots.offset, mul(index, 0x20)))) }
            if (value != bytes32(0)) return false;
        }
        return true;
    }

    function slotsMatch(bytes32[] calldata slots, bytes32[] calldata expected) external view returns (bool) {
        if (slots.length != expected.length) return false;
        for (uint256 index; index < slots.length; ++index) {
            bytes32 value;
            assembly ("memory-safe") { value := sload(calldataload(add(slots.offset, mul(index, 0x20)))) }
            if (value != expected[index]) return false;
        }
        return true;
    }
}

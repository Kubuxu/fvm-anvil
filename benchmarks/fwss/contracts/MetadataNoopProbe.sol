// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

/// @notice Lab-only controls for KAMT same-value storage writes at derived metadata addresses.
contract MetadataNoopProbe {
    address public immutable operator;

    constructor(address operator_) { operator = operator_; }

    /// @notice Modes: 0 read header, 1 same header write, 2 clear payload, 3 clear payload and same header write.
    function probe(uint32 dataSetId, uint32 pieceId, bytes calldata key, uint8 mode, bytes32 expectedHeader)
        external returns (bytes32 observed)
    {
        require(msg.sender == operator && mode <= 3, "invalid probe");
        bytes32 dataset = keccak256(abi.encode(uint256(dataSetId), uint256(13)));
        bytes32 piece = keccak256(abi.encode(uint256(pieceId), dataset));
        bytes32 header = keccak256(abi.encodePacked(key, piece));
        assembly ("memory-safe") { observed := sload(header) }
        require(observed == expectedHeader, "header mismatch");
        if (mode >= 2) {
            require(uint256(observed) & 1 == 1, "long value required");
            uint256 words = ((uint256(observed) >> 1) + 31) / 32;
            uint256 base = uint256(keccak256(abi.encode(header)));
            for (uint256 i; i < words; ++i) {
                assembly ("memory-safe") { sstore(add(base, i), 0) }
            }
        }
        if (mode == 1 || mode == 3) {
            assembly ("memory-safe") { sstore(header, expectedHeader) }
        }
    }
}

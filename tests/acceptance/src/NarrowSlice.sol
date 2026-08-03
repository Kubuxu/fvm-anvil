// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.30;

contract NarrowSlice {
    uint256 public value;

    event ValueChanged(address indexed caller, uint256 previousValue, uint256 newValue);

    constructor(uint256 initialValue) {
        value = initialValue;
    }

    function setValue(uint256 newValue) external returns (uint256 previousValue) {
        previousValue = value;
        value = newValue;
        emit ValueChanged(msg.sender, previousValue, newValue);
    }
}

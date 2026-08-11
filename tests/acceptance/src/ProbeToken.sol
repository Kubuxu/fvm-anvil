// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

contract ProbeToken {
    uint256[107] private gap;
    mapping(address => uint256) private balances;
    mapping(address => mapping(address => uint256)) private allowances;

    constructor(address holder) {
        balances[holder] = 1_000;
    }

    function balanceOf(address account) external view returns (uint256) {
        return balances[account];
    }

    function allowance(address owner, address spender) external view returns (uint256) {
        return allowances[owner][spender];
    }

    function transfer(address recipient, uint256 amount) external returns (bool) {
        balances[msg.sender] -= amount;
        balances[recipient] += amount;
        return true;
    }

    function transferFrom(address owner, address recipient, uint256 amount) external returns (bool) {
        allowances[owner][msg.sender] -= amount;
        balances[owner] -= amount;
        balances[recipient] += amount;
        return true;
    }
}

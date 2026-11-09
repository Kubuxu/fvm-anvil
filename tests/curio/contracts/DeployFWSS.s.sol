// SPDX-License-Identifier: Apache-2.0 OR MIT
pragma solidity 0.8.37;

import {Script} from "forge-std/Script.sol";
import {FWSSDeployer} from "./lib/FWSSDeployer.sol";
import {PDPVerifier} from "@pdp/PDPVerifier.sol";
import {MyERC1967Proxy} from "@pdp/ERC1967Proxy.sol";
import {ServiceProviderRegistry} from "../src/ServiceProviderRegistry.sol";
import {FilecoinPayV1} from "@fws-payments/FilecoinPayV1.sol";
import {SessionKeyRegistry} from "@session-key-registry/SessionKeyRegistry.sol";
import {WETH} from "solady/tokens/WETH.sol";

/// Fresh local stack using the checked-out upstream deployer, never a historical archive.
contract DeployFWSS is Script, FWSSDeployer {
    function run() external returns (string memory manifest) {
        address owner = vm.envAddress("DEPLOY_OWNER");
        vm.startBroadcast();
        address token = vm.envOr("DEPLOY_TOKEN", address(0));
        if (token == address(0)) {
            WETH wrapped = new WETH();
            wrapped.deposit{value: vm.envUint("DEPLOY_WRAP_AMOUNT")}();
            token = address(wrapped);
        }
        address sessions = address(new SessionKeyRegistry());
        address payments = address(new FilecoinPayV1());
        address pdpImplementation = address(new PDPVerifier(1, vm.envUint("DEPLOY_FINALITY")));
        address pdp = address(new MyERC1967Proxy(pdpImplementation, abi.encodeCall(PDPVerifier.initialize, ())));
        address registryImplementation = address(new ServiceProviderRegistry(1));
        address registry = address(new MyERC1967Proxy(registryImplementation, abi.encodeCall(ServiceProviderRegistry.initialize, ())));
        address dispatcher = _deployDispatcher();
        address fwss = address(_deployFWSS(FWSSDeployment({
            payments: payments, pdpVerifier: pdp, usdfc: token,
            filBeamBeneficiary: owner, serviceProviderRegistry: registry,
            sessionKeyRegistry: sessions, maxProvingPeriod: uint64(vm.envUint("DEPLOY_PERIOD")),
            challengeWindowSize: vm.envUint("DEPLOY_WINDOW"), filBeamController: owner
        }), dispatcher));
        vm.stopBroadcast();
        string memory key = "contracts";
        vm.serializeAddress(key, "token", token);
        vm.serializeAddress(key, "sessionKeys", sessions);
        vm.serializeAddress(key, "payments", payments);
        vm.serializeAddress(key, "pdpImplementation", pdpImplementation);
        vm.serializeAddress(key, "pdp", pdp);
        vm.serializeAddress(key, "registryImplementation", registryImplementation);
        vm.serializeAddress(key, "registry", registry);
        vm.serializeAddress(key, "fwssDispatcher", dispatcher);
        vm.serializeAddress(key, "fwss", fwss);
        manifest = vm.serializeAddress(key, "view", fwss);
    }
}

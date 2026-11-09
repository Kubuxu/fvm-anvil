// SPDX-License-Identifier: Apache-2.0 OR MIT
pragma solidity 0.8.37;

import {Script} from "forge-std/Script.sol";
import {FWSSDeployer} from "./lib/FWSSDeployer.sol";
import {Migration, SetDelegateOperation, SetDelegateOperationLibrary} from "@erc8167/lib/Migration.sol";
import {IFilecoinWarmStorageService} from "../src/interfaces/IFilecoinWarmStorageService.sol";
import {FWSSStorageTermsMigration} from "../src/FWSSStorageTermsMigration.sol";

/// Deploy only candidate facets and migrations: the imported proxy, dispatcher,
/// dependencies, and proving configuration are deliberately not redeployed.
contract UpgradeFWSS is Script, FWSSDeployer {
    function run() external returns (string memory manifest) {
        IFilecoinWarmStorageService fwss = IFilecoinWarmStorageService(vm.envAddress("UPGRADE_FWSS"));
        _setConstructorArg("_paymentsContractAddress", fwss.paymentsContractAddress());
        _setConstructorArg("_pdpVerifierAddress", fwss.pdpVerifierAddress());
        _setConstructorArg("_usdfc", address(fwss.usdfcTokenAddress()));
        _setConstructorArg("_filBeamBeneficiaryAddress", fwss.filBeamBeneficiaryAddress());
        _setConstructorArg("_serviceProviderRegistry", address(fwss.serviceProviderRegistry()));
        _setConstructorArg("_sessionKeyRegistry", address(fwss.sessionKeyRegistry()));
        bytes4[] memory oldSelectors = fwss.selectors();
        Facet[] memory facets = _resolveFacets(MAINNET_INDEX);

        vm.startBroadcast();
        SetDelegateOperation[] memory routes = _deployFacetRoutes(facets);
        SetDelegateOperationLibrary.validate(routes);
        // A complete cutover also removes exports absent from the candidate.
        SetDelegateOperation[] memory operations = new SetDelegateOperation[](routes.length + oldSelectors.length);
        uint256 count = routes.length;
        for (uint256 i; i < routes.length; ++i) operations[i] = routes[i];
        for (uint256 i; i < oldSelectors.length; ++i) {
            bool retained;
            for (uint256 j; j < routes.length; ++j) {
                if (oldSelectors[i] == routes[j].selector) {
                    retained = true;
                    break;
                }
            }
            if (!retained) operations[count++] = SetDelegateOperation(oldSelectors[i], address(0));
        }
        assembly ("memory-safe") {
            mstore(operations, count)
        }
        SetDelegateOperationLibrary.validate(operations);
        address rawMigration = Migration.createMigration(operations);
        address wrapper = address(new FWSSStorageTermsMigration(rawMigration));
        vm.stopBroadcast();

        string memory routeJson;
        string memory runtimeJson;
        for (uint256 i; i < routes.length; ++i) {
            routeJson = vm.serializeAddress("candidate-routes", vm.toString(abi.encodePacked(routes[i].selector)), routes[i].delegate);
            runtimeJson = vm.serializeBytes("candidate-runtimes", vm.toString(routes[i].delegate), routes[i].delegate.code);
        }
        runtimeJson = vm.serializeBytes("candidate-runtimes", vm.toString(rawMigration), rawMigration.code);
        runtimeJson = vm.serializeBytes("candidate-runtimes", vm.toString(wrapper), wrapper.code);
        string memory artifactJson;
        string memory constructorJson;
        uint256 next;
        for (uint256 i; i < facets.length; ++i) {
            require(facets[i].selectors.length != 0, "Facet without exports");
            address delegate = routes[next].delegate;
            artifactJson = vm.serializeString("candidate-artifacts", vm.toString(delegate), facets[i].artifact);
            constructorJson = vm.serializeBytes("candidate-constructors", vm.toString(delegate), _facetConstructorArgs(facets[i]));
            next += facets[i].selectors.length;
        }
        manifest = string.concat(
            "{\"routes\":", routeJson, ",\"runtimes\":", runtimeJson,
            ",\"artifacts\":", artifactJson, ",\"constructorArgs\":", constructorJson,
            ",\"rawMigration\":\"", vm.toString(rawMigration),
            "\",\"migration\":\"", vm.toString(wrapper), "\"}"
        );
    }
}

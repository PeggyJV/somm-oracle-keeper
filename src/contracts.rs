use alloy::sol;

sol! {
    #[sol(rpc)]
    interface ISharePriceOracle {
        function performUpkeep(bytes calldata performData) external;
        function heartbeat() external view returns (uint64);
        function gracePeriod() external view returns (uint64);
        function observationsLength() external view returns (uint16);
        function currentIndex() external view returns (uint16);
        function observations(uint256 index) external view returns (uint64 timestamp, uint192 cumulative);
        function getLatest() external view returns (uint256 ans, uint256 twaa, bool notSafeToUse);
        function killSwitch() external view returns (bool);
        function automationForwarder() external view returns (address);
        function target() external view returns (address);
    }

    #[sol(rpc)]
    interface IVault {
        function totalSupply() external view returns (uint256);
        function totalAssets() external view returns (uint256);
    }

    #[sol(rpc)]
    interface ISafe {
        function nonce() external view returns (uint256);
        function getThreshold() external view returns (uint256);
        function isOwner(address owner) external view returns (bool);
        function getTransactionHash(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            uint256 _nonce
        ) external view returns (bytes32);
        function execTransaction(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes calldata signatures
        ) external payable returns (bool);
    }
}

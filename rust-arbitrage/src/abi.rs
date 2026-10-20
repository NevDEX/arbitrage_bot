//! Typed contract calls and events. Router calls use SwapRouter02 tuple layouts.
use alloy::sol;
sol! {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function factory() external view returns (address);
    function fee() external view returns (uint24);
    function getPool(address tokenA,address tokenB,uint24 fee) external view returns (address);
    function decimals() external view returns (uint8);
    function liquidity() external view returns (uint128);
    function slot0() external view returns (uint160 sqrtPriceX96,int24 tick,uint16 observationIndex,uint16 observationCardinality,uint16 observationCardinalityNext,uint8 feeProtocol,bool unlocked);
    function vault() external view returns (address);
    function exchange() external view returns (address);
    function balanceOf(address owner) external view returns (uint256);
    function balances(address token,address owner) external view returns (uint256);
    function allowance(address owner,address spender) external view returns (uint256);
    struct QuoteParams { address tokenIn; address tokenOut; uint256 amount; uint24 fee; uint160 sqrtPriceLimitX96; }
    function quoteExactInputSingle(QuoteParams params) external returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate);
    function quoteExactOutputSingle(QuoteParams params) external returns (uint256 amountIn,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate);
    struct SwapParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amount; uint256 bound; uint160 sqrtPriceLimitX96; }
    function exactInputSingle(SwapParams params) external returns (uint256);
    function exactOutputSingle(SwapParams params) external returns (uint256);
    function multicall(uint256 deadline,bytes[] data) external returns (bytes[]);
    struct AddressSet { address baseToken; address quoteToken; address relayer; }
    struct MatchResult { address maker; address taker; address buyer; uint256 makerFee; uint256 makerRebate; uint256 takerFee; uint256 makerGasFee; uint256 takerGasFee; uint256 baseTokenFilledAmount; uint256 quoteTokenFilledAmount; }
    event Match(AddressSet addressSet,MatchResult result);
    event Transfer(address indexed from,address indexed to,uint256 value);
}

package arbitrage

import (
	"context"
	"fmt"
	"math/big"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/crypto"
)

// https://github.com/Uniswap/contracts/blob/main/deployments/4663.md
const robinhoodV3Factory = "0x1f7d7550B1b028f7571E69A784071F0205FD2EfA"

type uniswapReference struct {
	rpc    *RPC
	config ArbitrageConfig
	market Market
}

func (r *RPC) contractRead(ctx context.Context, address, signature, block string, args ...[]byte) ([]byte, error) {
	data := append([]byte{}, crypto.Keccak256([]byte(signature))[:4]...)
	for _, arg := range args {
		data = append(data, common.LeftPadBytes(arg, 32)...)
	}
	var raw string
	if err := r.call(ctx, "eth_call", []interface{}{map[string]string{"to": address, "data": hexutil.Encode(data)}, block}, &raw); err != nil {
		return nil, err
	}
	value, err := hexutil.Decode(raw)
	if err != nil || len(value) == 0 || len(value)%32 != 0 {
		return nil, fmt.Errorf("invalid Uniswap contract response")
	}
	return value, nil
}

func readWordAddress(raw []byte) (common.Address, error) {
	if len(raw) != 32 || new(big.Int).SetBytes(raw[:12]).Sign() != 0 {
		return common.Address{}, fmt.Errorf("invalid Uniswap address response")
	}
	return common.BytesToAddress(raw), nil
}

func (u *uniswapReference) check(ctx context.Context, block string) error {
	readAddress := func(address, method string) (common.Address, error) {
		raw, err := u.rpc.contractRead(ctx, address, method, block)
		if err != nil {
			return common.Address{}, err
		}
		return readWordAddress(raw)
	}
	pool := u.config.UniswapV3Pool
	t0, err := readAddress(pool, "token0()")
	if err != nil {
		return err
	}
	t1, err := readAddress(pool, "token1()")
	if err != nil {
		return err
	}
	base, quote := common.HexToAddress(u.market.BaseTokenAddress), common.HexToAddress(u.market.QuoteTokenAddress)
	if !(t0 == base && t1 == quote || t0 == quote && t1 == base) {
		return fmt.Errorf("Uniswap pool tokens do not match market")
	}
	factory, err := readAddress(pool, "factory()")
	if err != nil {
		return err
	}
	if factory != common.HexToAddress(robinhoodV3Factory) {
		return fmt.Errorf("Uniswap pool factory does not match official deployment")
	}
	fee, err := u.rpc.contractRead(ctx, pool, "fee()", block)
	if err != nil {
		return err
	}
	if len(fee) != 32 || new(big.Int).SetBytes(fee).BitLen() > 24 {
		return fmt.Errorf("invalid Uniswap pool fee")
	}
	registered, err := u.rpc.contractRead(ctx, factory.Hex(), "getPool(address,address,uint24)", block, t0.Bytes(), t1.Bytes(), fee)
	if err != nil {
		return err
	}
	a, err := readWordAddress(registered)
	if err != nil || a != common.HexToAddress(pool) {
		return fmt.Errorf("Uniswap pool is not registered in factory")
	}
	for i, address := range []common.Address{base, quote} {
		raw, err := u.rpc.contractRead(ctx, address.Hex(), "decimals()", block)
		if err != nil {
			return err
		}
		expected := *u.market.BaseTokenDecimals
		if i == 1 {
			expected = *u.market.QuoteTokenDecimals
		}
		if len(raw) != 32 || new(big.Int).SetBytes(raw).Cmp(big.NewInt(int64(expected))) != 0 {
			return fmt.Errorf("Uniswap token decimals differ from market configuration")
		}
	}
	return nil
}

func (u *uniswapReference) readSqrtPrice(ctx context.Context, block string) (*big.Int, error) {
	state, err := u.rpc.contractRead(ctx, u.config.UniswapV3Pool, "slot0()", block)
	if err != nil {
		return nil, err
	}
	if len(state) != 224 || new(big.Int).SetBytes(state[192:224]).Cmp(big.NewInt(1)) != 0 {
		return nil, fmt.Errorf("invalid or locked Uniswap pool state")
	}
	sqrt := new(big.Int).SetBytes(state[:32])
	if sqrt.Sign() <= 0 || sqrt.BitLen() > 160 {
		return nil, fmt.Errorf("invalid pool sqrt price")
	}
	liquidity, err := u.rpc.contractRead(ctx, u.config.UniswapV3Pool, "liquidity()", block)
	if err != nil {
		return nil, err
	}
	if len(liquidity) != 32 || new(big.Int).SetBytes(liquidity).Sign() <= 0 || new(big.Int).SetBytes(liquidity).BitLen() > 128 {
		return nil, fmt.Errorf("Uniswap pool has no active liquidity")
	}
	return sqrt, nil
}

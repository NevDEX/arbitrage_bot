package arbitrage

import (
	"context"
	"crypto/ecdsa"
	"fmt"
	"math/big"
	"sort"
	"strings"
	"time"

	"github.com/ethereum/go-ethereum/accounts/abi"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/crypto"
)

const arbRouter = "0xcaf681a66d020601342297493863e78c959e5cb2"
const arbQuoter = "0x33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7"

// Official SwapRouter02 uses deadline-free swap tuples; multicall supplies the deadline.
var arbV3ABI = arbParseABI(`[
 {"type":"function","name":"quoteExactInputSingle","inputs":[{"name":"params","type":"tuple","components":[{"name":"tokenIn","type":"address"},{"name":"tokenOut","type":"address"},{"name":"amountIn","type":"uint256"},{"name":"fee","type":"uint24"},{"name":"sqrtPriceLimitX96","type":"uint160"}]}],"outputs":[{"type":"uint256"},{"type":"uint160"},{"type":"uint32"},{"type":"uint256"}]},
 {"type":"function","name":"quoteExactOutputSingle","inputs":[{"name":"params","type":"tuple","components":[{"name":"tokenIn","type":"address"},{"name":"tokenOut","type":"address"},{"name":"amount","type":"uint256"},{"name":"fee","type":"uint24"},{"name":"sqrtPriceLimitX96","type":"uint160"}]}],"outputs":[{"type":"uint256"},{"type":"uint160"},{"type":"uint32"},{"type":"uint256"}]},
 {"type":"function","name":"exactInputSingle","inputs":[{"name":"params","type":"tuple","components":[{"name":"tokenIn","type":"address"},{"name":"tokenOut","type":"address"},{"name":"fee","type":"uint24"},{"name":"recipient","type":"address"},{"name":"amountIn","type":"uint256"},{"name":"amountOutMinimum","type":"uint256"},{"name":"sqrtPriceLimitX96","type":"uint160"}]}],"outputs":[{"type":"uint256"}]},
 {"type":"function","name":"exactOutputSingle","inputs":[{"name":"params","type":"tuple","components":[{"name":"tokenIn","type":"address"},{"name":"tokenOut","type":"address"},{"name":"fee","type":"uint24"},{"name":"recipient","type":"address"},{"name":"amountOut","type":"uint256"},{"name":"amountInMaximum","type":"uint256"},{"name":"sqrtPriceLimitX96","type":"uint160"}]}],"outputs":[{"type":"uint256"}]},
 {"type":"function","name":"multicall","inputs":[{"name":"deadline","type":"uint256"},{"name":"data","type":"bytes[]"}],"outputs":[{"type":"bytes[]"}]}
]`)

var arbMatchABI = arbParseABI(`[{"type":"event","name":"Match","anonymous":false,"inputs":[
 {"name":"addressSet","type":"tuple","components":[{"name":"baseToken","type":"address"},{"name":"quoteToken","type":"address"},{"name":"relayer","type":"address"}]},
 {"name":"result","type":"tuple","components":[{"name":"maker","type":"address"},{"name":"taker","type":"address"},{"name":"buyer","type":"address"},{"name":"makerFee","type":"uint256"},{"name":"makerRebate","type":"uint256"},{"name":"takerFee","type":"uint256"},{"name":"makerGasFee","type":"uint256"},{"name":"takerGasFee","type":"uint256"},{"name":"baseTokenFilledAmount","type":"uint256"},{"name":"quoteTokenFilledAmount","type":"uint256"}]}]}]`)

func arbParseABI(s string) abi.ABI {
	a, err := abi.JSON(strings.NewReader(s))
	if err != nil {
		panic(err)
	}
	return a
}

type arbBlock struct {
	Number    string         `json:"number"`
	Hash      common.Hash    `json:"hash"`
	Timestamp hexutil.Uint64 `json:"timestamp"`
}

func (b arbBlock) number() uint64 { n, _ := hexutil.DecodeUint64(b.Number); return n }

func (r *RPC) arbBlock(ctx context.Context, tag string) (arbBlock, error) {
	var b arbBlock
	err := r.call(ctx, "eth_getBlockByNumber", []interface{}{tag, false}, &b)
	_, parseErr := hexutil.DecodeUint64(b.Number)
	if err == nil && (parseErr != nil || b.Hash == (common.Hash{}) || b.Timestamp == 0) {
		err = fmt.Errorf("invalid chain block")
	}
	return b, err
}

func (r *RPC) arbHead(ctx context.Context, c ArbitrageConfig) (arbBlock, error) {
	b, err := r.arbBlock(ctx, "latest")
	if err != nil {
		return b, err
	}
	if uint64(b.Timestamp) > uint64(time.Now().Unix()+5) || time.Now().Unix()-int64(b.Timestamp) > int64(c.MaxDataAgeSeconds) {
		return b, fmt.Errorf("RPC head is stale or clock is incorrect")
	}
	return b, nil
}

type arbV3 struct {
	rpc       *RPC
	cfg       ArbitrageConfig
	market    Market
	reference *uniswapReference
	fee       *big.Int
}

func (u *arbV3) check(ctx context.Context, head arbBlock, live bool) error {
	var chain string
	if err := u.rpc.call(ctx, "eth_chainId", []interface{}{}, &chain); err != nil {
		return err
	}
	id, err := hexutil.DecodeUint64(chain)
	if err != nil || id != uint64(u.cfg.ChainID) {
		return fmt.Errorf("RPC chain mismatch")
	}
	u.reference = &uniswapReference{rpc: u.rpc, config: u.cfg, market: u.market}
	if err = u.reference.check(ctx, head.Number); err != nil {
		return err
	}
	raw, err := u.rpc.contractRead(ctx, u.cfg.UniswapV3Pool, "fee()", head.Number)
	if err != nil {
		return err
	}
	u.fee = new(big.Int).SetBytes(raw)
	for _, address := range []string{arbRouter, arbQuoter} {
		raw, err := u.rpc.contractRead(ctx, address, "factory()", head.Number)
		if err != nil {
			return err
		}
		factory, err := readWordAddress(raw)
		if err != nil || factory != common.HexToAddress(robinhoodV3Factory) {
			return fmt.Errorf("Uniswap Router/Quoter factory mismatch")
		}
	}
	if _, err = u.reference.readSqrtPrice(ctx, head.Number); err != nil {
		return err
	}
	if !live {
		return nil
	}
	var code string
	if err = u.rpc.call(ctx, "eth_getCode", []interface{}{u.cfg.VaultAddress, head.Number}, &code); err != nil {
		return err
	}
	bytes, err := hexutil.Decode(code)
	if err != nil || len(bytes) == 0 {
		return fmt.Errorf("Vault has no runtime")
	}
	raw, err = u.rpc.contractRead(ctx, u.cfg.ExchangeAddress, "vault()", head.Number)
	if err != nil {
		return err
	}
	vault, err := readWordAddress(raw)
	if err != nil || vault != common.HexToAddress(u.cfg.VaultAddress) {
		return fmt.Errorf("Exchange uses another Vault")
	}
	raw, err = u.rpc.contractRead(ctx, u.cfg.VaultAddress, "exchange()", head.Number)
	if err != nil {
		return err
	}
	exchange, err := readWordAddress(raw)
	if err != nil || exchange != common.HexToAddress(u.cfg.ExchangeAddress) {
		return fmt.Errorf("Vault is not bound to the configured Exchange")
	}
	return nil
}

type arbQuoteInput struct {
	TokenIn, TokenOut                common.Address
	AmountIn, Fee, SqrtPriceLimitX96 *big.Int
}

type arbQuoteOutput struct {
	TokenIn, TokenOut              common.Address
	Amount, Fee, SqrtPriceLimitX96 *big.Int
}

func (u *arbV3) quote(ctx context.Context, side string, base *big.Int, head arbBlock) (*big.Int, error) {
	var data []byte
	var err error
	method := "quoteExactInputSingle"
	if side == "sell" {
		data, err = arbV3ABI.Pack(method, arbQuoteInput{common.HexToAddress(u.cfg.BaseToken), common.HexToAddress(u.cfg.QuoteToken), base, u.fee, new(big.Int)})
	} else {
		method = "quoteExactOutputSingle"
		data, err = arbV3ABI.Pack(method, arbQuoteOutput{common.HexToAddress(u.cfg.QuoteToken), common.HexToAddress(u.cfg.BaseToken), base, u.fee, new(big.Int)})
	}
	if err != nil {
		return nil, err
	}
	var raw hexutil.Bytes
	if err = u.rpc.call(ctx, "eth_call", []interface{}{map[string]string{"to": arbQuoter, "data": hexutil.Encode(data)}, head.Number}, &raw); err != nil {
		return nil, err
	}
	values, err := arbV3ABI.Unpack(method, raw)
	if err != nil || len(values) != 4 {
		return nil, fmt.Errorf("invalid QuoterV2 response")
	}
	amount := values[0].(*big.Int)
	sqrt := values[1].(*big.Int)
	// Quotes that exhaust the pool at TickMath's limits may consume only part of an exact input.
	max, _ := new(big.Int).SetString("1461446703485210103287273052203988822378723970341", 10)
	if amount.Sign() <= 0 || sqrt.Cmp(big.NewInt(4295128740)) <= 0 || sqrt.Cmp(max) >= 0 {
		return nil, fmt.Errorf("Uniswap quote has no full-size liquidity")
	}
	return amount, nil
}

func (u *arbV3) funds(ctx context.Context, address, side string, base, quote *big.Int, block string) error {
	token, amount := u.cfg.BaseToken, base
	if side == "buy" {
		token, amount = u.cfg.QuoteToken, quote
	}
	for _, method := range []string{"balanceOf(address)", "allowance(address,address)"} {
		args := [][]byte{common.HexToAddress(address).Bytes()}
		if strings.HasPrefix(method, "allowance") {
			args = append(args, common.HexToAddress(arbRouter).Bytes())
		}
		raw, err := u.rpc.contractRead(ctx, token, method, block, args...)
		if err != nil {
			return err
		}
		if len(raw) != 32 || new(big.Int).SetBytes(raw).Cmp(amount) < 0 {
			return fmt.Errorf("Uniswap wallet balance or bounded Router allowance is insufficient")
		}
	}
	var native hexutil.Big
	if err := u.rpc.call(ctx, "eth_getBalance", []interface{}{address, block}, &native); err != nil {
		return err
	}
	cap, err := arbUnits(u.cfg.MaxHedgeGasNative, 18)
	if err != nil {
		return err
	}
	if (*big.Int)(&native).Cmp(cap) < 0 {
		return fmt.Errorf("insufficient native gas reserve")
	}
	return nil
}

type arbAddresses struct{ BaseToken, QuoteToken, Relayer common.Address }
type arbMatch struct {
	Maker, Taker, Buyer                                                                                      common.Address
	MakerFee, MakerRebate, TakerFee, MakerGasFee, TakerGasFee, BaseTokenFilledAmount, QuoteTokenFilledAmount *big.Int
}

func arbDecodeFill(raw types.Log, c ArbitrageConfig, address string, active *arbOrder) (*arbFill, error) {
	values, err := arbMatchABI.Unpack("Match", raw.Data)
	if err != nil || len(values) != 2 {
		return nil, fmt.Errorf("invalid Match event")
	}
	a := *abi.ConvertType(values[0], new(arbAddresses)).(*arbAddresses)
	r := *abi.ConvertType(values[1], new(arbMatch)).(*arbMatch)
	owner := common.HexToAddress(address)
	if a.BaseToken != common.HexToAddress(c.BaseToken) || a.QuoteToken != common.HexToAddress(c.QuoteToken) || r.Maker != owner && r.Taker != owner {
		return nil, nil
	}
	if active == nil || a.Relayer != common.HexToAddress(active.Relayer) || r.Maker == r.Taker || (r.Buyer != r.Maker && r.Buyer != r.Taker) {
		return nil, fmt.Errorf("unexpected wallet fill; dedicate this wallet to one arbitrage instance")
	}
	buy := r.Buyer == owner
	if buy != (active.Quote.Side == "buy") || r.BaseTokenFilledAmount.Sign() <= 0 || r.QuoteTokenFilledAmount.Sign() <= 0 {
		return nil, fmt.Errorf("settled fill differs from active arbitrage direction")
	}
	fee := new(big.Int).Add(r.TakerFee, r.TakerGasFee)
	if r.Maker == owner {
		fee.Sub(new(big.Int).Add(r.MakerFee, r.MakerGasFee), r.MakerRebate)
	}
	delta := new(big.Int).Sub(r.QuoteTokenFilledAmount, fee)
	side := "buy"
	if buy {
		delta.Neg(new(big.Int).Add(r.QuoteTokenFilledAmount, fee))
		side = "sell"
	}
	return &arbFill{ID: raw.TxHash.Hex() + ":" + fmt.Sprint(raw.Index), Side: side, Base: r.BaseTokenFilledAmount.String(), QuoteDelta: delta.String(), Block: raw.BlockNumber, BlockHash: raw.BlockHash.Hex()}, nil
}

// Commit the entire scan batch and its fills together so restarts cannot lose a partial fill.
func (u *arbV3) scan(ctx context.Context, j *arbJournal, address string, head arbBlock) (bool, arbBlock, error) {
	s := j.state
	s.Pending = append([]arbFill{}, s.Pending...)
	if s.Active != nil {
		copy := *s.Active
		s.Active = &copy
	}
	previous, err := u.rpc.arbBlock(ctx, hexutil.EncodeUint64(s.Cursor))
	if err != nil {
		return false, arbBlock{}, err
	}
	if previous.Hash != common.HexToHash(s.CursorHash) {
		return false, previous, fmt.Errorf("settlement cursor reorg; reconcile before restarting")
	}
	if head.number() < uint64(u.cfg.Confirmations) || head.number()-uint64(u.cfg.Confirmations) <= s.Cursor {
		return true, previous, nil
	}
	end := head.number() - uint64(u.cfg.Confirmations)
	caughtUp := true
	if end-s.Cursor > 500 {
		end = s.Cursor + 500
		caughtUp = false
	}
	last, err := u.rpc.arbBlock(ctx, hexutil.EncodeUint64(end))
	if err != nil {
		return false, last, err
	}
	var logs []types.Log
	if err = u.rpc.call(ctx, "eth_getLogs", []interface{}{map[string]interface{}{"address": u.cfg.ExchangeAddress, "fromBlock": hexutil.EncodeUint64(s.Cursor + 1), "toBlock": hexutil.EncodeUint64(end), "topics": []string{arbMatchABI.Events["Match"].ID.Hex()}}}, &logs); err != nil {
		return false, last, err
	}
	sort.Slice(logs, func(i, k int) bool {
		if logs[i].BlockNumber != logs[k].BlockNumber {
			return logs[i].BlockNumber < logs[k].BlockNumber
		}
		return logs[i].Index < logs[k].Index
	})
	seen := map[string]bool{}
	for _, f := range s.Pending {
		seen[f.ID] = true
	}
	for _, raw := range logs {
		if raw.Removed || raw.Address != common.HexToAddress(u.cfg.ExchangeAddress) || raw.BlockNumber <= s.Cursor || raw.BlockNumber > end || raw.TxHash == (common.Hash{}) || len(raw.Topics) != 1 || raw.Topics[0] != arbMatchABI.Events["Match"].ID {
			return false, last, fmt.Errorf("invalid settlement log identity")
		}
		fill, err := arbDecodeFill(raw, u.cfg, address, s.Active)
		if err != nil {
			return false, last, err
		}
		if fill == nil || seen[fill.ID] {
			continue
		}
		block, err := u.rpc.arbBlock(ctx, hexutil.EncodeUint64(raw.BlockNumber))
		if err != nil {
			return false, last, err
		}
		if block.Hash != raw.BlockHash {
			return false, last, fmt.Errorf("settlement event reorg")
		}
		filled := new(big.Int).Add(arbInt(s.Active.FilledBase), arbInt(fill.Base))
		max, err := arbUnits(s.Active.Quote.Amount.String(), *u.market.BaseTokenDecimals)
		if err != nil || filled.Cmp(max) > 0 {
			return false, last, fmt.Errorf("settled fills exceed the persisted order amount")
		}
		s.Active.FilledBase = filled.String()
		s.Active.Seen = true
		s.Pending = append(s.Pending, *fill)
		seen[fill.ID] = true
	}
	if len(s.Pending) > 100 {
		return false, last, fmt.Errorf("unhedged fill queue exceeds limit")
	}
	again, err := u.rpc.arbBlock(ctx, hexutil.EncodeUint64(end))
	if err != nil {
		return false, last, err
	}
	if again.Hash != last.Hash {
		return false, last, fmt.Errorf("scan block changed during read")
	}
	s.Cursor = end
	s.CursorHash = last.Hash.Hex()
	j.state = s
	return caughtUp, last, j.save()
}

type arbSwapInput struct {
	TokenIn, TokenOut                             common.Address
	Fee                                           *big.Int
	Recipient                                     common.Address
	AmountIn, AmountOutMinimum, SqrtPriceLimitX96 *big.Int
}

type arbSwapOutput struct {
	TokenIn, TokenOut                             common.Address
	Fee                                           *big.Int
	Recipient                                     common.Address
	AmountOut, AmountInMaximum, SqrtPriceLimitX96 *big.Int
}

func (u *arbV3) submit(ctx context.Context, j *arbJournal, key *ecdsa.PrivateKey, address string, head arbBlock) error {
	f := j.state.Pending[0]
	base := arbInt(f.Base)
	price, err := u.quote(ctx, f.Side, base, head)
	if err != nil {
		return err
	}
	bound := arbSlip(price, u.cfg.SlippageBPS, f.Side == "buy")
	cap, _ := arbUnits(u.cfg.MaxQuotePerTrade, *u.market.QuoteTokenDecimals)
	if price.Cmp(cap) > 0 || bound.Cmp(cap) > 0 || bound.Sign() <= 0 {
		return fmt.Errorf("hedge exceeds per-trade quote cap")
	}
	if err = u.funds(ctx, address, f.Side, base, bound, head.Number); err != nil {
		return err
	}
	var swap []byte
	if f.Side == "sell" {
		swap, err = arbV3ABI.Pack("exactInputSingle", arbSwapInput{common.HexToAddress(u.cfg.BaseToken), common.HexToAddress(u.cfg.QuoteToken), u.fee, common.HexToAddress(address), base, bound, new(big.Int)})
	} else {
		swap, err = arbV3ABI.Pack("exactOutputSingle", arbSwapOutput{common.HexToAddress(u.cfg.QuoteToken), common.HexToAddress(u.cfg.BaseToken), u.fee, common.HexToAddress(address), base, bound, new(big.Int)})
	}
	if err != nil {
		return err
	}
	deadline := uint64(head.Timestamp) + 60
	data, err := arbV3ABI.Pack("multicall", new(big.Int).SetUint64(deadline), [][]byte{swap})
	if err != nil {
		return err
	}
	call := map[string]string{"from": address, "to": arbRouter, "data": hexutil.Encode(data), "value": "0x0"}
	var simulation hexutil.Bytes
	if err = u.rpc.call(ctx, "eth_call", []interface{}{call, head.Number}, &simulation); err != nil {
		return err
	}
	var estimate, nonce hexutil.Uint64
	var gasPrice hexutil.Big
	if err = u.rpc.call(ctx, "eth_estimateGas", []interface{}{call}, &estimate); err != nil {
		return err
	}
	if estimate == 0 || estimate > 833333 {
		return fmt.Errorf("hedge gas estimate exceeds hard cap")
	}
	gas := uint64(estimate)*120/100 + 1
	if err = u.rpc.call(ctx, "eth_gasPrice", []interface{}{}, &gasPrice); err != nil {
		return err
	}
	gp := (*big.Int)(&gasPrice)
	maxPrice, _ := arbUnits(u.cfg.MaxGasPriceGwei, 9)
	maxGas, _ := arbUnits(u.cfg.MaxHedgeGasNative, 18)
	if gp.Sign() <= 0 || gp.Cmp(maxPrice) > 0 || new(big.Int).Mul(new(big.Int).SetUint64(gas), gp).Cmp(maxGas) > 0 {
		return fmt.Errorf("hedge gas exceeds configured cap")
	}
	if err = u.rpc.call(ctx, "eth_getTransactionCount", []interface{}{address, "pending"}, &nonce); err != nil {
		return err
	}
	if time.Now().Unix()-int64(head.Timestamp) > int64(u.cfg.MaxDataAgeSeconds) {
		return fmt.Errorf("hedge quote became stale")
	}
	tx := types.NewTransaction(uint64(nonce), common.HexToAddress(arbRouter), new(big.Int), gas, gp, data)
	signed, err := types.SignTx(tx, types.LatestSignerForChainID(big.NewInt(u.cfg.ChainID)), key)
	if err != nil {
		return fmt.Errorf("hedge signing failed")
	}
	raw, err := signed.MarshalBinary()
	if err != nil {
		return err
	}
	budget, _ := arbUnits(u.cfg.GasCostQuote, *u.market.QuoteTokenDecimals)
	// Persist only the public hash/nonce, never the signature or signed raw payload.
	j.state.Tx = &arbHedgeTx{Hash: signed.Hash().Hex(), Nonce: uint64(nonce), Deadline: deadline, FillID: f.ID, Bound: bound.String(), GasBudget: budget.String()}
	if err = j.save(); err != nil {
		return err
	}
	var hash common.Hash
	if err = u.rpc.call(ctx, "eth_sendRawTransaction", []interface{}{hexutil.Encode(raw)}, &hash); err != nil || hash != signed.Hash() {
		return fmt.Errorf("hedge broadcast uncertain: %s; reconcile original hash and nonce before replacement", signed.Hash().Hex())
	}
	return nil
}

var arbTransferTopic = crypto.Keccak256Hash([]byte("Transfer(address,address,uint256)"))

func arbTransferDelta(receipt *types.Receipt, token, address common.Address) (*big.Int, error) {
	delta := new(big.Int)
	for _, l := range receipt.Logs {
		if l.Address != token || len(l.Topics) == 0 || l.Topics[0] != arbTransferTopic {
			continue
		}
		if len(l.Topics) != 3 || len(l.Data) != 32 {
			return nil, fmt.Errorf("malformed ERC-20 transfer event")
		}
		value := new(big.Int).SetBytes(l.Data)
		if common.BytesToAddress(l.Topics[2].Bytes()[12:]) == address {
			delta.Add(delta, value)
		}
		if common.BytesToAddress(l.Topics[1].Bytes()[12:]) == address {
			delta.Sub(delta, value)
		}
	}
	return delta, nil
}

func (u *arbV3) settle(ctx context.Context, j *arbJournal, address string, head arbBlock) error {
	tx := j.state.Tx
	var receipt *types.Receipt
	if err := u.rpc.call(ctx, "eth_getTransactionReceipt", []interface{}{tx.Hash}, &receipt); err != nil {
		return err
	}
	if receipt == nil {
		if uint64(head.Timestamp) > tx.Deadline+60 {
			return fmt.Errorf("hedge remains unresolved: %s; inspect original nonce", tx.Hash)
		}
		return nil
	}
	if receipt.TxHash != common.HexToHash(tx.Hash) || receipt.Status != 1 || receipt.BlockNumber == nil || !receipt.BlockNumber.IsUint64() {
		return fmt.Errorf("hedge failed or receipt mismatched: %s; fill retained", tx.Hash)
	}
	block, err := u.rpc.arbBlock(ctx, hexutil.EncodeUint64(receipt.BlockNumber.Uint64()))
	if err != nil {
		return err
	}
	if block.Hash != receipt.BlockHash {
		return fmt.Errorf("hedge receipt reorg")
	}
	if head.number() < block.number()+uint64(u.cfg.Confirmations) {
		return nil
	}
	if len(j.state.Pending) == 0 || j.state.Pending[0].ID != tx.FillID {
		return fmt.Errorf("hedge fill identity mismatch")
	}
	f := j.state.Pending[0]
	owner := common.HexToAddress(address)
	baseDelta, err := arbTransferDelta(receipt, common.HexToAddress(u.cfg.BaseToken), owner)
	if err != nil {
		return err
	}
	quoteDelta, err := arbTransferDelta(receipt, common.HexToAddress(u.cfg.QuoteToken), owner)
	if err != nil {
		return err
	}
	want := arbInt(f.Base)
	if f.Side == "sell" {
		want.Neg(want)
	}
	if baseDelta.Cmp(want) != 0 || f.Side == "sell" && quoteDelta.Cmp(arbInt(tx.Bound)) < 0 || f.Side == "buy" && (quoteDelta.Sign() >= 0 || new(big.Int).Neg(quoteDelta).Cmp(arbInt(tx.Bound)) > 0) {
		return fmt.Errorf("hedge actual transfer mismatch; reconcile residual exposure")
	}
	pnl := new(big.Int).Sub(new(big.Int).Add(arbInt(f.QuoteDelta), quoteDelta), arbInt(tx.GasBudget))
	j.state.PnL = new(big.Int).Add(arbInt(j.state.PnL), pnl).String()
	if pnl.Sign() < 0 {
		j.state.Loss = new(big.Int).Sub(arbInt(j.state.Loss), pnl).String()
	}
	j.state.Pending = j.state.Pending[1:]
	j.state.Tx = nil
	return j.save()
}

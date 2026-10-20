package arbitrage

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"math/big"
	"net/http"
	"strings"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/shopspring/decimal"
)

type RPC struct {
	url   string
	http  *http.Client
	vault common.Address
}

func NewRPC(c ArbitrageConfig) *RPC {
	return &RPC{c.RPCURL, newHTTPClient(c.TimeoutSeconds), common.HexToAddress(c.VaultAddress)}
}

func (r *RPC) call(ctx context.Context, method string, params interface{}, out interface{}) error {
	b, _ := json.Marshal(map[string]interface{}{"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
	q, e := http.NewRequestWithContext(ctx, "POST", r.url, bytes.NewReader(b))
	if e != nil {
		return fmt.Errorf("invalid RPC URL")
	}
	q.Header.Set("Content-Type", "application/json")
	resp, e := r.http.Do(q)
	if e != nil {
		return fmt.Errorf("RPC %s unavailable", method)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return fmt.Errorf("RPC %s HTTP status=%d", method, resp.StatusCode)
	}
	raw, e := readLimited(resp.Body, 1<<20)
	if e != nil {
		return fmt.Errorf("RPC response unreadable")
	}
	var result struct {
		Result json.RawMessage `json:"result"`
		Error  json.RawMessage `json:"error"`
	}
	if json.Unmarshal(raw, &result) != nil || len(result.Error) > 0 && string(result.Error) != "null" || len(result.Result) == 0 {
		return fmt.Errorf("RPC %s failed", method)
	}
	if json.Unmarshal(result.Result, out) != nil {
		return fmt.Errorf("invalid RPC %s result", method)
	}
	return nil
}

type Balance struct{ AvailableBase, AvailableQuote decimal.Decimal }

func (r *RPC) Balances(ctx context.Context, m Market, address string, locks map[string]string, market string, minBlock uint64) (Balance, error) {
	var b Balance
	var block string
	if e := r.call(ctx, "eth_blockNumber", []interface{}{}, &block); e != nil {
		return b, e
	}
	head, ok := new(big.Int).SetString(strings.TrimPrefix(block, "0x"), 16)
	if !ok || head.Sign() < 0 || head.Cmp(new(big.Int).SetUint64(minBlock)) < 0 {
		return b, fmt.Errorf("RPC head is behind settlement")
	}
	symbols := strings.Split(market, "-")
	available := make([]decimal.Decimal, 0, 2)
	for i, token := range []string{m.BaseTokenAddress, m.QuoteTokenAddress} {
		data := crypto.Keccak256([]byte("balances(address,address)"))[:4]
		data = append(data, common.LeftPadBytes(common.HexToAddress(token).Bytes(), 32)...)
		data = append(data, common.LeftPadBytes(common.HexToAddress(address).Bytes(), 32)...)
		var raw string
		if e := r.call(ctx, "eth_call", []interface{}{map[string]string{"to": r.vault.Hex(), "data": fmt.Sprintf("0x%x", data)}, block}, &raw); e != nil {
			return b, e
		}
		if len(raw) != 66 || !strings.HasPrefix(raw, "0x") {
			return b, fmt.Errorf("invalid vault balance result")
		}
		amount, ok := new(big.Int).SetString(raw[2:], 16)
		if !ok {
			return b, fmt.Errorf("invalid vault balance")
		}
		locked, exists := locks[symbols[i]]
		if !exists {
			return b, fmt.Errorf("locked balance token is missing")
		}
		lockedInt, ok := new(big.Int).SetString(locked, 10)
		if !ok || lockedInt.Sign() < 0 {
			return b, fmt.Errorf("invalid locked balance")
		}
		decimals := *m.BaseTokenDecimals
		if i == 1 {
			decimals = *m.QuoteTokenDecimals
		}
		total := decimal.NewFromBigInt(amount, -int32(decimals))
		free := decimal.Max(decimal.Zero, total.Sub(decimal.NewFromBigInt(lockedInt, -int32(decimals))))
		available = append(available, free)
	}
	b = Balance{AvailableBase: available[0], AvailableQuote: available[1]}
	return b, nil
}

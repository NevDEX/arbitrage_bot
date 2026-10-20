package arbitrage

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math/big"
	"strings"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/shopspring/decimal"
)

type Order struct {
	ID        string          `json:"Id"`
	Trader    string          `json:"TraderAddress"`
	Market    string          `json:"MarketId"`
	Side      string          `json:"Side"`
	Type      string          `json:"Type"`
	Status    string          `json:"Status"`
	Price     decimal.Decimal `json:"Price"`
	Amount    decimal.Decimal `json:"Amount"`
	Available decimal.Decimal `json:"AvailableAmount"`
	Pending   decimal.Decimal `json:"PendingAmount"`
}

// Only fixed field names and reasons reach logs; never echo response values.
type orderFieldError struct{ field, reason string }

func (e *orderFieldError) Error() string {
	return fmt.Sprintf("openOrder.%s %s; backend must return numeric order quantities (rebuild/restart server if using an older version)", e.field, e.reason)
}

func (o *Order) UnmarshalJSON(raw []byte) error {
	type plainOrder Order
	var decoded Order
	wire := struct {
		*plainOrder
		Price     json.RawMessage `json:"Price"`
		Amount    json.RawMessage `json:"Amount"`
		Available json.RawMessage `json:"AvailableAmount"`
		Pending   json.RawMessage `json:"PendingAmount"`
	}{plainOrder: (*plainOrder)(&decoded)}
	if err := json.Unmarshal(raw, &wire); err != nil {
		return err
	}
	for _, field := range []struct {
		name  string
		raw   json.RawMessage
		value *decimal.Decimal
	}{
		{"Price", wire.Price, &decoded.Price},
		{"Amount", wire.Amount, &decoded.Amount},
		{"AvailableAmount", wire.Available, &decoded.Available},
		{"PendingAmount", wire.Pending, &decoded.Pending},
	} {
		value := bytes.TrimSpace(field.raw)
		if len(value) == 0 || bytes.Equal(value, []byte("null")) || bytes.Equal(value, []byte(`""`)) {
			return &orderFieldError{field.name, "is missing or empty"}
		}
		if err := json.Unmarshal(value, field.value); err != nil {
			return &orderFieldError{field.name, "is not a valid decimal"}
		}
	}
	*o = decoded
	return nil
}

func validateOpenOrders(orders []Order, market, address string) ([]Order, error) {
	seen := map[string]bool{}
	for _, o := range orders {
		if o.Market != market || !strings.EqualFold(o.Trader, address) || !validID(o.ID) || seen[o.ID] || (o.Side != "buy" && o.Side != "sell") || (o.Status != "pending" && o.Status != "settling") || (o.Type != "limit" && o.Type != "market") || !o.Price.IsPositive() || !o.Amount.IsPositive() || o.Available.IsNegative() || o.Pending.IsNegative() || o.Available.Add(o.Pending).GreaterThan(o.Amount) {
			return nil, fmt.Errorf("order recovery returned an invalid owner, market or ID")
		}
		seen[o.ID] = true
	}
	return orders, nil
}

type BuiltOrder struct {
	ID     string          `json:"id"`
	Market string          `json:"marketID"`
	Side   string          `json:"side"`
	Type   string          `json:"type"`
	Price  decimal.Decimal `json:"price"`
	Amount decimal.Decimal `json:"amount"`
	Gas    decimal.Decimal `json:"gasFeeAmount"`
	JSON   struct {
		Trader      string `json:"trader"`
		Relayer     string `json:"relayer"`
		Base        string `json:"baseToken"`
		Quote       string `json:"quoteToken"`
		BaseAmount  string `json:"baseTokenAmount"`
		QuoteAmount string `json:"quoteTokenAmount"`
		Gas         string `json:"gasTokenAmount"`
		Data        string `json:"data"`
	} `json:"json"`
}

func builtHash(o BuiltOrder) (string, error) {
	typeHash := crypto.Keccak256([]byte("Order(address trader,address relayer,address baseToken,address quoteToken,uint256 baseTokenAmount,uint256 quoteTokenAmount,uint256 gasTokenAmount,bytes32 data)"))
	words := append([]byte(nil), typeHash...)
	for _, s := range []string{o.JSON.Trader, o.JSON.Relayer, o.JSON.Base, o.JSON.Quote} {
		if !nonzeroAddress(s) {
			return "", fmt.Errorf("invalid order token or signer")
		}
		words = append(words, common.LeftPadBytes(common.HexToAddress(s).Bytes(), 32)...)
	}
	for _, s := range []string{o.JSON.BaseAmount, o.JSON.QuoteAmount, o.JSON.Gas} {
		n, ok := new(big.Int).SetString(s, 10)
		if !ok || n.Sign() < 0 || n.BitLen() > 256 {
			return "", fmt.Errorf("invalid integer order amount")
		}
		words = append(words, common.LeftPadBytes(n.Bytes(), 32)...)
	}
	data, e := hexutil.Decode(o.JSON.Data)
	if e != nil || len(data) != 32 {
		return "", fmt.Errorf("invalid order data")
	}
	words = append(words, data...)
	domain := crypto.Keccak256(crypto.Keccak256([]byte("EIP712Domain(string name)")), crypto.Keccak256([]byte("NevDEX")))
	return hexutil.Encode(crypto.Keccak256([]byte{0x19, 0x01}, domain, crypto.Keccak256(words))), nil
}

// Validate the complete crossing order before signing its ID.
func validateBuilt(o BuiltOrder, q Quote, c ArbitrageConfig, m Market, address string, now time.Time) (int64, error) {
	if !validID(o.ID) || o.Market != c.Market || o.Side != q.Side || o.Type != "limit" || !o.Price.Equal(q.Price) || !o.Amount.Equal(q.Amount) || !o.Gas.IsZero() {
		return 0, fmt.Errorf("build response does not match requested quote")
	}
	if !strings.EqualFold(o.JSON.Trader, address) || !strings.EqualFold(o.JSON.Base, c.BaseToken) || !strings.EqualFold(o.JSON.Quote, c.QuoteToken) || o.JSON.BaseAmount != q.Amount.Shift(int32(*m.BaseTokenDecimals)).Truncate(0).String() || o.JSON.QuoteAmount != q.Amount.Mul(q.Price).Shift(int32(*m.QuoteTokenDecimals)).Truncate(0).String() || o.JSON.Gas != "0" {
		return 0, fmt.Errorf("build response changed order tokens or amounts")
	}
	data, e := hexutil.Decode(o.JSON.Data)
	if e != nil || len(data) != 32 {
		return 0, fmt.Errorf("invalid signed data")
	}
	side := byte(0)
	if q.Side == "sell" {
		side = 1
	}
	if data[0] != 2 || data[1] != side || data[2] != 0 {
		return 0, fmt.Errorf("unexpected order version, side or type")
	}
	if data[22] != 0 {
		return 0, fmt.Errorf("unexpected post-only flag in crossing order")
	}
	if data[12] != 0 || data[13] != 0 {
		return 0, fmt.Errorf("unexpected maker rebate encoding")
	}
	expiry := int64(new(big.Int).SetBytes(data[3:8]).Uint64())
	if expiry < now.Unix()+30 || expiry > now.Unix()+int64(c.OrderTTLSeconds)+10 {
		return 0, fmt.Errorf("backend returned an unsafe order expiry; update the server expiry fixes")
	}
	for i, rate := range []decimal.Decimal{m.MakerFeeRate, m.TakerFeeRate} {
		want := rate.Shift(5).Truncate(0)
		if !want.Equal(decimal.NewFromInt(int64(binary.BigEndian.Uint16(data[8+i*2 : 10+i*2])))) {
			return 0, fmt.Errorf("signed fee differs from market fee")
		}
	}
	hash, e := builtHash(o)
	if e != nil || !bytes.Equal(common.FromHex(hash), common.FromHex(o.ID)) {
		return 0, fmt.Errorf("built order hash mismatch")
	}
	return expiry, nil
}

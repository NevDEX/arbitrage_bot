package arbitrage

import (
	"fmt"
	"math/big"
	"time"

	"github.com/shopspring/decimal"
)

// All monetary calculations use integer token units or decimal strings, never float64.
type ArbitrageOpportunity struct {
	Route      string    `json:"route"`
	Quote      Quote     `json:"order"`
	Cost       string    `json:"costRaw"`
	Proceeds   string    `json:"proceedsRaw"`
	Net        string    `json:"netProfitRaw"`
	Commitment string    `json:"commitmentRaw"`
	CreatedAt  time.Time `json:"createdAt"`
}

func arbUnits(s string, decimals int) (*big.Int, error) {
	d, err := decimal.NewFromString(s)
	if err != nil || d.IsNegative() || !d.Shift(int32(decimals)).Equal(d.Shift(int32(decimals)).Truncate(0)) {
		return nil, fmt.Errorf("amount cannot be represented at token precision")
	}
	return d.Shift(int32(decimals)).BigInt(), nil
}

func arbInt(s string) *big.Int { n, _ := new(big.Int).SetString(s, 10); return n }

func arbCeil(n, d *big.Int) *big.Int {
	return new(big.Int).Div(new(big.Int).Add(n, new(big.Int).Sub(d, big.NewInt(1))), d)
}

func arbFee(n *big.Int, m Market) *big.Int {
	rate := decimal.Max(m.MakerFeeRate, m.TakerFeeRate).Shift(5).Truncate(0).BigInt()
	return arbCeil(new(big.Int).Mul(n, rate), big.NewInt(100000))
}

func arbSlip(n *big.Int, bps int64, buy bool) *big.Int {
	if buy {
		return arbCeil(new(big.Int).Mul(n, big.NewInt(10000+bps)), big.NewInt(10000))
	}
	return new(big.Int).Div(new(big.Int).Mul(n, big.NewInt(10000-bps)), big.NewInt(10000))
}

// Walk the visible ladder at the actual size. A best price alone does not prove depth.
func bookLimitPrice(levels [][2]string, quantity *big.Int, m Market) (decimal.Decimal, error) {
	remaining := new(big.Int).Set(quantity)
	var limit decimal.Decimal
	for _, level := range levels {
		price, err := arbUnits(level[0], *m.PriceDecimals)
		if err != nil || price.Sign() <= 0 {
			return limit, fmt.Errorf("invalid book price precision")
		}
		amount, err := arbUnits(level[1], *m.BaseTokenDecimals)
		if err != nil || amount.Sign() <= 0 {
			return limit, fmt.Errorf("invalid book quantity")
		}
		if amount.Cmp(remaining) > 0 {
			amount.Set(remaining)
		}
		remaining.Sub(remaining, amount)
		limit, _ = decimal.NewFromString(level[0])
		if remaining.Sign() == 0 {
			return limit, nil
		}
	}
	return limit, fmt.Errorf("insufficient visible order-book depth")
}

func arbPlan(c ArbitrageConfig, m Market, depth Depth, uniSell, uniBuy *big.Int, now time.Time) (*ArbitrageOpportunity, error) {
	var book Book
	if err := book.Update(depth, now); err != nil {
		return nil, err
	}
	base, err := arbUnits(c.OrderAmount, *m.BaseTokenDecimals)
	if err != nil || base.Sign() <= 0 || !dec(c.OrderAmount).Equal(dec(c.OrderAmount).Truncate(int32(*m.AmountDecimals))) {
		return nil, fmt.Errorf("orderAmount does not match market precision")
	}
	if uniSell == nil || uniBuy == nil || uniSell.Sign() <= 0 || uniBuy.Sign() <= 0 {
		return nil, fmt.Errorf("invalid executable Uniswap quote")
	}
	gas, err := arbUnits(c.GasCostQuote, *m.QuoteTokenDecimals)
	if err != nil {
		return nil, err
	}
	minimum, err := arbUnits(c.MinProfitQuote, *m.QuoteTokenDecimals)
	if err != nil {
		return nil, err
	}
	cap, err := arbUnits(c.MaxQuotePerTrade, *m.QuoteTokenDecimals)
	if err != nil {
		return nil, err
	}
	var best *ArbitrageOpportunity
	for _, side := range []string{"sell", "buy"} {
		levels := depth.Bids
		if side == "buy" {
			levels = depth.Asks
		}
		limit, err := bookLimitPrice(levels, base, m)
		if err != nil {
			continue
		}
		if !limit.Mul(dec(c.OrderAmount)).GreaterThanOrEqual(m.MinOrderSize) {
			continue
		}
		// Reserve at the limit price, including the larger possible signed fee.
		limitRaw := limit.Mul(dec(c.OrderAmount)).Shift(int32(*m.QuoteTokenDecimals)).Truncate(0).BigInt()
		if limitRaw.Sign() <= 0 {
			continue
		}
		// Matching may happen after the visible levels change. Require profit even
		// if the whole order executes at our worst permitted limit price.
		gross := new(big.Int).Set(limitRaw)
		orderCap := new(big.Int).Add(limitRaw, arbFee(limitRaw, m))
		cost, proceeds, route := arbSlip(uniBuy, c.SlippageBPS, true), new(big.Int).Sub(gross, arbFee(gross, m)), "BUY_UNI_SELL_MYOB"
		if side == "buy" {
			cost = new(big.Int).Add(gross, arbFee(gross, m))
			proceeds = arbSlip(uniSell, c.SlippageBPS, false)
			route = "BUY_MYOB_SELL_UNI"
		}
		// A small integer cushion covers per-level rounding in the visible ladder.
		cost.Add(cost, big.NewInt(int64(len(levels))))
		net := new(big.Int).Sub(new(big.Int).Sub(proceeds, cost), gas)
		commitment := new(big.Int).Set(orderCap)
		if commitment.Cmp(cost) < 0 {
			commitment.Set(cost)
		}
		if commitment.Cmp(cap) > 0 || proceeds.Cmp(cap) > 0 || net.Cmp(minimum) < 0 ||
			decimal.NewFromBigInt(net, 0).LessThan(decimal.NewFromBigInt(cost, 0).Mul(dec(c.MinProfitPct))) {
			continue
		}
		o := &ArbitrageOpportunity{Route: route, Quote: Quote{Side: side, Price: limit, Amount: dec(c.OrderAmount)},
			Cost: cost.String(), Proceeds: proceeds.String(), Net: net.String(), Commitment: commitment.String(), CreatedAt: now}
		if best == nil || net.Cmp(arbInt(best.Net)) > 0 {
			best = o
		}
	}
	return best, nil
}

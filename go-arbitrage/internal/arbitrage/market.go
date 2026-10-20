package arbitrage

import (
	"fmt"
	"strings"

	"github.com/shopspring/decimal"
)

type Market struct {
	ID                 string          `json:"id"`
	IsPublished        bool            `json:"isPublished"`
	BaseTokenAddress   string          `json:"baseTokenAddress"`
	QuoteTokenAddress  string          `json:"quoteTokenAddress"`
	BaseTokenDecimals  *int            `json:"baseTokenDecimals"`
	QuoteTokenDecimals *int            `json:"quoteTokenDecimals"`
	PriceDecimals      *int            `json:"priceDecimals"`
	AmountDecimals     *int            `json:"amountDecimals"`
	MinOrderSize       decimal.Decimal `json:"minOrderSize"`
	MakerFeeRate       decimal.Decimal `json:"makerFeeRate"`
	TakerFeeRate       decimal.Decimal `json:"takerFeeRate"`
}

func (m Market) validate(c ArbitrageConfig) error {
	if m.ID != c.Market || !m.IsPublished || !strings.EqualFold(m.BaseTokenAddress, c.BaseToken) || !strings.EqualFold(m.QuoteTokenAddress, c.QuoteToken) {
		return fmt.Errorf("market/token deployment does not match configuration")
	}
	for _, p := range []*int{m.BaseTokenDecimals, m.QuoteTokenDecimals, m.PriceDecimals, m.AmountDecimals} {
		if p == nil || *p < 0 || *p > 18 {
			return fmt.Errorf("market precision missing or invalid")
		}
	}
	if *m.AmountDecimals > *m.BaseTokenDecimals {
		return fmt.Errorf("order amount precision exceeds base token precision")
	}
	if m.MinOrderSize.IsNegative() || m.MakerFeeRate.IsNegative() || m.TakerFeeRate.IsNegative() || m.MakerFeeRate.Shift(5).Truncate(0).GreaterThan(decimal.NewFromInt(65535)) || m.TakerFeeRate.Shift(5).Truncate(0).GreaterThan(decimal.NewFromInt(65535)) {
		return fmt.Errorf("invalid market fee or minimum")
	}
	return nil
}

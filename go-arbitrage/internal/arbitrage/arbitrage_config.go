package arbitrage

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/shopspring/decimal"
)

var (
	marketPattern  = regexp.MustCompile(`^[A-Za-z0-9]+-[A-Za-z0-9]+$`)
	decimalPattern = regexp.MustCompile(`^\d+(\.\d+)?$`)
)

// ArbitrageConfig deliberately has no inline private-key field or implicit live mode.
type ArbitrageConfig struct {
	Endpoint                string `json:"endpoint"`
	RPCURL                  string `json:"rpcURL"`
	ChainID                 int64  `json:"chainID"`
	Market                  string `json:"market"`
	BaseToken               string `json:"baseToken"`
	QuoteToken              string `json:"quoteToken"`
	UniswapV3Pool           string `json:"uniswapV3Pool"`
	VaultAddress            string `json:"vaultAddress"`
	ExchangeAddress         string `json:"exchangeAddress"`
	Account                 string `json:"account"`
	PrivateKeyFile          string `json:"privateKeyFile"`
	StateDir                string `json:"stateDir"`
	OrderAmount             string `json:"orderAmount"`
	MinProfitPct            string `json:"minProfitPct"`
	MinProfitQuote          string `json:"minProfitQuote"`
	GasCostQuote            string `json:"gasCostQuote"`
	MaxQuotePerTrade        string `json:"maxQuotePerTrade"`
	MaxTotalQuote           string `json:"maxTotalQuote"`
	MaxLossQuote            string `json:"maxLossQuote"`
	MaxFeeRate              string `json:"maxFeeRate"`
	MaxGasPriceGwei         string `json:"maxGasPriceGwei"`
	MaxHedgeGasNative       string `json:"maxHedgeGasNative"`
	SlippageBPS             int64  `json:"slippageBPS"`
	Confirmations           int    `json:"confirmations"`
	PollMilliseconds        int    `json:"pollMilliseconds"`
	CancelAfterMilliseconds int    `json:"cancelAfterMilliseconds"`
	OrderTTLSeconds         int    `json:"orderTTLSeconds"`
	TimeoutSeconds          int    `json:"timeoutSeconds"`
	MaxDataAgeSeconds       int    `json:"maxDataAgeSeconds"`
}

func defaultArbitrageConfig() ArbitrageConfig {
	return ArbitrageConfig{
		Endpoint: "https://nevdex.xyz/api/", RPCURL: "https://rpc.mainnet.chain.robinhood.com", ChainID: 4663,
		Market: "CASHCAT-USDG", BaseToken: "0x020bfC650A365f8BB26819deAAbF3E21291018b4",
		QuoteToken: "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168", UniswapV3Pool: "0x4b0c312ffbb068f6a0bea128759e35d94b94d0e1",
		StateDir: ".arbitrage-state", OrderAmount: "0.1", MinProfitPct: "0.005", MinProfitQuote: "0.0001",
		GasCostQuote: "0.001", MaxQuotePerTrade: "1", MaxTotalQuote: "10", MaxLossQuote: "0.1", MaxFeeRate: "0.01",
		MaxGasPriceGwei: "100", MaxHedgeGasNative: "0.001", SlippageBPS: 50, Confirmations: 3,
		PollMilliseconds: 1000, CancelAfterMilliseconds: 1500, OrderTTLSeconds: 60, TimeoutSeconds: 10, MaxDataAgeSeconds: 30,
	}
}

func LoadArbitrageConfig(file string) (ArbitrageConfig, error) {
	c := defaultArbitrageConfig()
	f, err := os.Open(file)
	if err != nil {
		return c, fmt.Errorf("open arbitrage configuration: %w", err)
	}
	defer f.Close()
	raw, err := readLimited(f, 65536)
	if err != nil || len(bytes.TrimSpace(raw)) == 0 || bytes.TrimSpace(raw)[0] != '{' {
		return c, fmt.Errorf("configuration must be a JSON object of at most 64 KiB")
	}
	d := json.NewDecoder(bytes.NewReader(raw))
	d.DisallowUnknownFields()
	if err := d.Decode(&c); err != nil {
		return c, fmt.Errorf("invalid arbitrage configuration (decimal values must be strings)")
	}
	if d.Decode(new(interface{})) != io.EOF {
		return c, fmt.Errorf("configuration must contain one JSON object")
	}
	base, err := filepath.Abs(filepath.Dir(file))
	if err != nil {
		return c, err
	}
	for _, p := range []*string{&c.StateDir, &c.PrivateKeyFile} {
		if *p != "" && !filepath.IsAbs(*p) {
			*p = filepath.Join(base, *p)
		}
	}
	return c, c.validate(false)
}

func (c ArbitrageConfig) validate(live bool) error {
	for _, raw := range []string{c.Endpoint, c.RPCURL} {
		u, err := url.Parse(raw)
		if err != nil || u.Host == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" ||
			(u.Scheme != "https" && !(u.Scheme == "http" && (u.Hostname() == "localhost" || u.Hostname() == "127.0.0.1" || u.Hostname() == "::1"))) {
			return fmt.Errorf("use HTTPS or loopback HTTP endpoints without credentials or query parameters")
		}
	}
	if c.ChainID != 4663 || !marketPattern.MatchString(c.Market) ||
		!nonzeroAddress(c.BaseToken) || !nonzeroAddress(c.QuoteToken) || strings.EqualFold(c.BaseToken, c.QuoteToken) || !nonzeroAddress(c.UniswapV3Pool) {
		return fmt.Errorf("configure a direct ERC-20 V3 market on chain 4663")
	}
	for name, s := range map[string]string{
		"orderAmount": c.OrderAmount, "minProfitPct": c.MinProfitPct, "minProfitQuote": c.MinProfitQuote,
		"gasCostQuote": c.GasCostQuote, "maxQuotePerTrade": c.MaxQuotePerTrade, "maxTotalQuote": c.MaxTotalQuote,
		"maxLossQuote": c.MaxLossQuote, "maxFeeRate": c.MaxFeeRate, "maxGasPriceGwei": c.MaxGasPriceGwei, "maxHedgeGasNative": c.MaxHedgeGasNative,
	} {
		n, err := decimal.NewFromString(s)
		if len(s) > 80 || !decimalPattern.MatchString(s) || err != nil || !n.IsPositive() {
			return fmt.Errorf("%s must be a positive decimal string", name)
		}
	}
	if dec(c.MinProfitPct).GreaterThanOrEqual(decimal.NewFromInt(1)) || dec(c.MaxFeeRate).GreaterThanOrEqual(decimal.NewFromInt(1)) ||
		dec(c.MaxQuotePerTrade).GreaterThan(dec(c.MaxTotalQuote)) {
		return fmt.Errorf("invalid profit, fee or total quote limit")
	}
	if c.SlippageBPS < 1 || c.SlippageBPS > 1000 || c.Confirmations < 1 || c.Confirmations > 100 ||
		c.PollMilliseconds < 250 || c.PollMilliseconds > 60000 || c.CancelAfterMilliseconds < 250 || c.CancelAfterMilliseconds > 10000 ||
		c.OrderTTLSeconds < 60 || c.OrderTTLSeconds > 120 || c.TimeoutSeconds < 1 || c.TimeoutSeconds > 30 ||
		c.MaxDataAgeSeconds < 2 || c.MaxDataAgeSeconds > 60 || c.StateDir == "" {
		return fmt.Errorf("invalid arbitrage timing, slippage or state directory")
	}
	if c.Account != "" && !nonzeroAddress(c.Account) {
		return fmt.Errorf("invalid account")
	}
	if live && (!nonzeroAddress(c.VaultAddress) || !nonzeroAddress(c.ExchangeAddress) || c.PrivateKeyFile == "") {
		return fmt.Errorf("live mode requires privateKeyFile, vaultAddress and exchangeAddress")
	}
	return nil
}

// validateMarket checks configured amounts against the market's token precision.
func (c ArbitrageConfig) validateMarket(m Market) error {
	if decimal.Max(m.MakerFeeRate, m.TakerFeeRate).GreaterThan(dec(c.MaxFeeRate)) {
		return fmt.Errorf("market fee exceeds configured maxFeeRate")
	}
	if _, err := arbUnits(c.OrderAmount, *m.BaseTokenDecimals); err != nil {
		return err
	}
	if !dec(c.OrderAmount).Equal(dec(c.OrderAmount).Truncate(int32(*m.AmountDecimals))) {
		return fmt.Errorf("orderAmount exceeds market amount precision")
	}
	for _, value := range []string{c.GasCostQuote, c.MinProfitQuote, c.MaxQuotePerTrade, c.MaxTotalQuote, c.MaxLossQuote} {
		if _, err := arbUnits(value, *m.QuoteTokenDecimals); err != nil {
			return err
		}
	}
	for _, value := range []struct {
		s        string
		decimals int
	}{{c.MaxGasPriceGwei, 9}, {c.MaxHedgeGasNative, 18}} {
		if _, err := arbUnits(value.s, value.decimals); err != nil {
			return err
		}
	}
	return nil
}

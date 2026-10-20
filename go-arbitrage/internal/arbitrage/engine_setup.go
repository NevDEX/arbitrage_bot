package arbitrage

import (
	"context"
	"fmt"
	"strings"

	"github.com/ethereum/go-ethereum/crypto"
	log "github.com/sirupsen/logrus"
)

func newArbitrageEngine(ctx context.Context, c ArbitrageConfig, live bool) (*ArbitrageEngine, error) {
	engine := &ArbitrageEngine{cfg: c, live: live}
	engine.api = NewAPI(c)
	engine.rpc = NewRPC(c)
	if err := engine.api.call(ctx, "getMarket", map[string]string{"marketID": c.Market}, &engine.market); err != nil {
		return nil, err
	}
	if err := engine.market.validate(c); err != nil {
		return nil, err
	}
	if err := c.validateMarket(engine.market); err != nil {
		return nil, err
	}
	head, err := engine.rpc.arbHead(ctx, c)
	if err != nil {
		return nil, err
	}
	engine.uni = &arbV3{rpc: engine.rpc, cfg: c, market: engine.market}
	if err = engine.uni.check(ctx, head, live); err != nil {
		return nil, err
	}
	log.Infof("arbitrage deployment checked market=%s chain=%d live=%t", c.Market, c.ChainID, live)
	return engine, nil
}

func (e *ArbitrageEngine) openWallet() error {
	c := e.cfg
	raw, err := arbPrivateRead(c.PrivateKeyFile, 4096)
	if err != nil {
		return err
	}
	key, err := crypto.HexToECDSA(strings.TrimPrefix(strings.TrimSpace(string(raw)), "0x"))
	if err != nil {
		return fmt.Errorf("invalid private-key file")
	}
	e.key = key
	e.address = crypto.PubkeyToAddress(key.PublicKey).Hex()
	if c.Account != "" && !strings.EqualFold(c.Account, e.address) {
		return fmt.Errorf("account differs from the private-key wallet")
	}
	e.journal, err = openArbJournal(c, e.address)
	if err != nil {
		return err
	}
	e.feed = newOrderFeed(c, e.address)
	return nil
}

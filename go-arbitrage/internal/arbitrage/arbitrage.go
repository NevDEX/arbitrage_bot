package arbitrage

import (
	"context"
	"crypto/ecdsa"
	"encoding/json"
	"fmt"
	"math/big"
	"strings"
	"sync"
	"time"

	"github.com/shopspring/decimal"
	log "github.com/sirupsen/logrus"
)

// ArbitrageEngine consumes events in one loop. Price readers may run concurrently,
// but a wallet never opens overlapping cycles or sends two independent hedge nonces.
type ArbitrageEngine struct {
	cfg     ArbitrageConfig
	market  Market
	api     *API
	rpc     *RPC
	uni     *arbV3
	feed    *orderFeed
	journal *arbJournal
	key     *ecdsa.PrivateKey
	address string
	depth   Depth
	book    Book
	live    bool
	events  <-chan StreamEvent
}

func RunArbitrage(ctx context.Context, c ArbitrageConfig, live, once, check bool) error {
	if live && (once || check) {
		return fmt.Errorf("preview/check cannot enable live trading")
	}
	if err := c.validate(live); err != nil {
		return err
	}
	if once || check {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, 30*time.Second)
		defer cancel()
	}
	engine, err := newArbitrageEngine(ctx, c, live)
	if err != nil {
		return err
	}
	if check {
		return nil
	}
	if live {
		if err := engine.openWallet(); err != nil {
			return err
		}
		defer engine.journal.close()
	}
	return engine.run(ctx, once)
}

func (e *ArbitrageEngine) run(ctx context.Context, once bool) error {
	c := e.cfg
	workCtx, cancel := context.WithCancel(ctx)
	var workers sync.WaitGroup
	events := make(chan StreamEvent, 32)
	e.events = events
	workers.Add(1)
	go func() { defer workers.Done(); watchMarket(workCtx, c, events) }()
	if e.live {
		workers.Add(1)
		go func() { defer workers.Done(); watchAccountOrders(workCtx, c, e.feed) }()
	}
	defer func() { cancel(); workers.Wait() }()
	defer e.shutdown()
	ticker := time.NewTicker(time.Duration(c.PollMilliseconds) * time.Millisecond)
	defer ticker.Stop()
	var lastStep time.Time
	for {
		select {
		case <-ctx.Done():
			return runContextError(ctx)
		case event := <-events:
			if err := e.marketEvent(event); err != nil {
				e.book.Ready = false
				log.Warnf("arbitrage market paused: %v", err)
			}
			if event.Type == "connected" {
				e.bootstrapBook(ctx)
				continue
			}
		case <-ticker.C:
		}
		if ctx.Err() != nil {
			return runContextError(ctx)
		}
		if time.Since(lastStep) < time.Duration(c.PollMilliseconds)*time.Millisecond {
			continue
		}
		lastStep = time.Now()
		preview, err := e.step(ctx)
		if err != nil {
			return err
		}
		if once && preview {
			return nil
		}
	}
}

func (e *ArbitrageEngine) marketEvent(event StreamEvent) error {
	if event.Market != e.cfg.Market {
		return fmt.Errorf("market event identity mismatch")
	}
	if event.ReceivedAt.IsZero() || time.Since(event.ReceivedAt) > time.Duration(e.cfg.MaxDataAgeSeconds)*time.Second {
		return fmt.Errorf("buffered market event is stale")
	}
	switch event.Type {
	case "connected":
		e.book.Ready = false
		e.depth = Depth{}
	case "disconnected":
		e.book.Ready = false
	case "heartbeat":
		if e.book.Ready {
			e.book.Contact = event.ReceivedAt
		}
	case "depth_update":
		var d Depth
		if json.Unmarshal(event.Data, &d) != nil {
			return fmt.Errorf("invalid depth snapshot")
		}
		return e.updateBook(d, event.ReceivedAt)
	}
	return nil
}

func (e *ArbitrageEngine) cancelActive(ctx context.Context, open []Order) error {
	o := e.journal.state.Active
	if o == nil {
		return nil
	}
	for _, row := range open {
		if !strings.EqualFold(row.ID, o.ID) {
			return fmt.Errorf("untracked wallet order; this wallet must be exclusive to arbitrage")
		}
		if !o.Seen {
			o.Seen = true
			if err := e.journal.save(); err != nil {
				return err
			}
		}
		unsafe := !e.book.fresh(e.cfg.MaxDataAgeSeconds)
		if row.Available.IsPositive() && (unsafe || time.Now().UnixMilli()-o.CreatedAt >= int64(e.cfg.CancelAfterMilliseconds)) {
			if err := e.cancelIntent(ctx); err != nil {
				return err
			}
		}
	}
	return nil
}

// A missing account stream does not prevent cancellation of our own durable ID.
// Cancellation never releases reservations or removes the settlement scan cursor.
func (e *ArbitrageEngine) cancelIntent(ctx context.Context) error {
	o := e.journal.state.Active
	if o == nil || time.Now().UnixMilli()-o.LastCancelAt < 5000 {
		return nil
	}
	o.LastCancelAt = time.Now().UnixMilli()
	if err := e.journal.save(); err != nil {
		return err
	}
	if err := e.api.Cancel(ctx, e.cfg.Market, e.address, o.ID); err != nil {
		log.Warnf("arbitrage cancellation uncertain order=%s; settlement scanning continues", o.ID)
		return nil
	}
	log.Infof("arbitrage cancellation queued order=%s; intent retained until expiry and confirmed scan", o.ID)
	return nil
}

func (e *ArbitrageEngine) step(ctx context.Context) (bool, error) {
	c := e.cfg
	head, err := e.rpc.arbHead(ctx, c)
	if err != nil {
		return false, err
	}
	if e.live {
		ready, err := e.reconcileCycle(ctx, head)
		if err != nil || !ready {
			return false, err
		}
	}

	if !e.book.fresh(c.MaxDataAgeSeconds) {
		return false, nil
	}
	base, _ := arbUnits(c.OrderAmount, *e.market.BaseTokenDecimals)
	quoteStarted := time.Now()
	if _, err = e.uni.reference.readSqrtPrice(ctx, head.Number); err != nil {
		return false, err
	}
	// The fixed-size executable quotes include Uniswap's pool fee and price impact.
	uniSell, err := e.uni.quote(ctx, "sell", base, head)
	if err != nil {
		return false, err
	}
	uniBuy, err := e.uni.quote(ctx, "buy", base, head)
	if err != nil {
		return false, err
	}
	opportunity, err := arbPlan(c, e.market, e.depth, uniSell, uniBuy, quoteStarted)
	if err != nil {
		return false, err
	}
	log.Infof("arbitrage preview market=%s base=%s uni_sell_quote=%s uni_buy_quote=%s opportunity=%t",
		c.Market, c.OrderAmount, decimal.NewFromBigInt(uniSell, -int32(*e.market.QuoteTokenDecimals)), decimal.NewFromBigInt(uniBuy, -int32(*e.market.QuoteTokenDecimals)), opportunity != nil)
	if opportunity != nil {
		log.Infof("arbitrage route=%s limit=%s estimated_net_quote=%s", opportunity.Route, opportunity.Quote.Price, decimal.NewFromBigInt(arbInt(opportunity.Net), -int32(*e.market.QuoteTokenDecimals)))
	}
	if !e.live || opportunity == nil {
		return true, nil
	}
	return true, e.place(ctx, *opportunity, head, base, uniBuy)
}

func (e *ArbitrageEngine) place(ctx context.Context, o ArbitrageOpportunity, head arbBlock, base, uniBuy *big.Int) error {
	c := e.cfg
	j := e.journal
	budget, _ := arbUnits(c.MaxTotalQuote, *e.market.QuoteTokenDecimals)
	commit := new(big.Int).Add(arbInt(j.state.Committed), arbInt(o.Commitment))
	if commit.Cmp(budget) > 0 {
		return fmt.Errorf("persistent total quote budget exhausted")
	}
	// Check the opposite venue's prefunded inventory before committing the first leg.
	hedgeSide := "sell"
	if o.Quote.Side == "sell" {
		hedgeSide = "buy"
	}
	if err := e.uni.funds(ctx, e.address, hedgeSide, base, arbSlip(uniBuy, c.SlippageBPS, true), head.Number); err != nil {
		return err
	}
	locks, err := e.api.Locks(ctx, e.address)
	if err != nil {
		return err
	}
	balance, err := e.rpc.Balances(ctx, e.market, e.address, locks, c.Market, head.number())
	if err != nil {
		return err
	}
	needed := o.Quote.Amount.Mul(o.Quote.Price)
	if o.Quote.Side == "buy" {
		needed = needed.Add(decimal.NewFromBigInt(arbFee(needed.Shift(int32(*e.market.QuoteTokenDecimals)).Truncate(0).BigInt(), e.market), -int32(*e.market.QuoteTokenDecimals)))
	}
	if o.Quote.Side == "buy" && balance.AvailableQuote.LessThan(needed) || o.Quote.Side == "sell" && balance.AvailableBase.LessThan(o.Quote.Amount) {
		return fmt.Errorf("insufficient available Vault inventory")
	}
	var built BuiltOrder
	if err = e.api.call(ctx, "order/build", map[string]interface{}{"marketID": c.Market, "address": e.address, "side": o.Quote.Side, "orderType": "limit", "postOnly": false, "price": o.Quote.Price.String(), "amount": o.Quote.Amount.String(), "expires": c.OrderTTLSeconds}, &built); err != nil {
		return err
	}
	expiry, err := validateBuilt(built, o.Quote, c, e.market, e.address, time.Unix(int64(head.Timestamp), 0))
	if err != nil {
		return err
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	// Recheck stream changes received while RPC/build requests were running.
	if err := e.drainMarketEvents(); err != nil {
		return err
	}
	if !e.book.fresh(c.MaxDataAgeSeconds) || time.Since(o.CreatedAt) > time.Duration(c.MaxDataAgeSeconds)*time.Second || time.Now().Unix()-int64(head.Timestamp) > int64(c.MaxDataAgeSeconds) {
		return fmt.Errorf("arbitrage opportunity became stale during build")
	}
	open, block, err := e.feed.snapshot()
	if err != nil || len(open) > 0 || block > head.number() {
		return fmt.Errorf("account state changed during build")
	}
	signature, err := signOrderID(built.ID, e.key)
	if err != nil {
		return err
	}
	// One durable intent, one placement attempt. A lost HTTP reply must not produce
	// a replacement order ID or an unrelated Uniswap swap.
	j.state.Active = &arbOrder{ID: built.ID, Relayer: built.JSON.Relayer, Quote: o.Quote, Expires: expiry, CreatedAt: time.Now().UnixMilli(), FilledBase: "0"}
	j.state.Committed = commit.String()
	if err = j.save(); err != nil {
		return err
	}
	if err = e.api.Place(ctx, e.address, built.ID, signature); err != nil {
		return fmt.Errorf("NevDEX placement uncertain: %s; retain original ID and inspect before restarting", built.ID)
	}
	log.Infof("arbitrage order queued id=%s side=%s price=%s amount=%s", built.ID, o.Quote.Side, o.Quote.Price, o.Quote.Amount)
	return nil
}

func (e *ArbitrageEngine) shutdown() {
	if !e.live || e.journal == nil || e.journal.state.Active == nil || e.feed == nil {
		return
	}
	// A separate, bounded context permits cancellation after the main context stops.
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := e.api.Cancel(ctx, e.cfg.Market, e.address, e.journal.state.Active.ID); err != nil {
		log.Warnf("arbitrage shutdown cancellation uncertain order=%s", e.journal.state.Active.ID)
	}
	log.Infof("arbitrage stopped; original order and hedge state retained; cancellation/settlement may still be pending")
}

func runContextError(ctx context.Context) error {
	if ctx.Err() == context.DeadlineExceeded {
		return fmt.Errorf("preview/check timed out before completion")
	}
	return nil
}

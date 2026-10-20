package arbitrage

import (
	"context"
	"fmt"
	"time"

	log "github.com/sirupsen/logrus"
)

// reconcileCycle finishes existing orders and hedges before allowing another cycle.
func (e *ArbitrageEngine) reconcileCycle(ctx context.Context, head arbBlock) (bool, error) {
	c := e.cfg
	open, block, accountErr := e.feed.snapshot()
	j := e.journal
	if j.state.CursorHash == "" {
		if accountErr != nil {
			return false, nil
		}
		if len(open) > 0 {
			return false, fmt.Errorf("first startup requires no existing wallet orders or settlements")
		}
		j.state.Cursor = head.number()
		j.state.CursorHash = head.Hash.Hex()
		if err := j.save(); err != nil {
			return false, err
		}
	}
	if accountErr == nil {
		if err := e.cancelActive(ctx, open); err != nil {
			return false, err
		}
	} else if err := e.cancelIntent(ctx); err != nil {
		return false, err
	}
	caughtUp, confirmed, err := e.uni.scan(ctx, j, e.address, head)
	if err != nil {
		return false, err
	}
	// Settlement scanning and hedge recovery continue without the market stream.
	if j.state.Tx != nil {
		hash := j.state.Tx.Hash
		if err = e.uni.settle(ctx, j, e.address, head); err != nil {
			return false, err
		}
		if j.state.Tx == nil {
			log.Infof("arbitrage hedge confirmed hash=%s estimated_pnl_raw=%s loss_raw=%s", hash, j.state.PnL, j.state.Loss)
		}
		return false, nil
	}
	if len(j.state.Pending) > 0 {
		if err = e.uni.submit(ctx, j, e.key, e.address, head); err != nil {
			return false, err
		}
		log.Infof("arbitrage hedge submitted fill=%s hash=%s", j.state.Pending[0].ID, j.state.Tx.Hash)
		return false, nil
	}
	if o := j.state.Active; o != nil {
		if !o.Seen && time.Now().UnixMilli()-o.CreatedAt > 30000 {
			return false, fmt.Errorf("unconfirmed NevDEX submission retained: %s; inspect original ID before restarting", o.ID)
		}
		// An empty snapshot/cancel ack alone cannot prove that an already matched
		// quantity will not settle later. Wait past signed expiry at a confirmed block.
		if !caughtUp || accountErr != nil || len(open) > 0 || !o.Seen || int64(confirmed.Timestamp) <= o.Expires {
			return false, nil
		}
		log.Infof("arbitrage cycle complete order=%s filled_base_raw=%s", o.ID, o.FilledBase)
		j.state.Active = nil
		if err := j.save(); err != nil {
			return false, err
		}
	}
	if accountErr != nil || !caughtUp || block > confirmed.number() {
		return false, nil
	}
	if len(open) > 0 {
		return false, fmt.Errorf("untracked wallet orders prevent a new arbitrage cycle")
	}
	loss, _ := arbUnits(c.MaxLossQuote, *e.market.QuoteTokenDecimals)
	if arbInt(j.state.Loss).Cmp(loss) >= 0 {
		return false, fmt.Errorf("cumulative estimated loss limit reached; no new orders")
	}
	return true, nil
}

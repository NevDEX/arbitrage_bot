package arbitrage

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/gorilla/websocket"
	"github.com/shopspring/decimal"
)

type Depth struct {
	Sequence uint64      `json:"sequence"`
	Bids     [][2]string `json:"bids"`
	Asks     [][2]string `json:"asks"`
}

type StreamEvent struct {
	Type       string          `json:"type"`
	Market     string          `json:"marketID"`
	Data       json.RawMessage `json:"data"`
	ReceivedAt time.Time       `json:"-"`
}

type Book struct {
	Ready   bool
	Contact time.Time
}

func (b Book) fresh(maxAgeSeconds int) bool {
	return b.Ready && time.Since(b.Contact) <= time.Duration(maxAgeSeconds)*time.Second
}

func (b *Book) Update(d Depth, now time.Time) error {
	for i, side := range [][][2]string{d.Bids, d.Asks} {
		var prev decimal.Decimal
		for n, level := range side {
			p, e := decimal.NewFromString(level[0])
			q, e2 := decimal.NewFromString(level[1])
			if e != nil || e2 != nil || !p.IsPositive() || !q.IsPositive() {
				return fmt.Errorf("invalid depth level")
			}
			if n > 0 && (i == 0 && !p.LessThan(prev) || i == 1 && !p.GreaterThan(prev)) {
				return fmt.Errorf("unsorted order book")
			}
			prev = p
		}
	}
	bid, ask := decimal.Zero, decimal.Zero
	if len(d.Bids) > 0 {
		bid, _ = decimal.NewFromString(d.Bids[0][0])
	}
	if len(d.Asks) > 0 {
		ask, _ = decimal.NewFromString(d.Asks[0][0])
	}
	if bid.IsPositive() && ask.IsPositive() && !bid.LessThan(ask) {
		return fmt.Errorf("crossed order book")
	}
	b.Ready = true
	b.Contact = now
	return nil
}

func socketURL(c ArbitrageConfig) string {
	u, _ := url.Parse(strings.TrimRight(c.Endpoint, "/") + "/v1/ws")
	if u.Scheme == "https" {
		u.Scheme = "wss"
	} else {
		u.Scheme = "ws"
	}
	q := u.Query()
	q.Set("marketID", c.Market)
	u.RawQuery = q.Encode()
	return u.String()
}

func watchMarket(ctx context.Context, c ArbitrageConfig, out chan<- StreamEvent) {
	send := func(e StreamEvent) bool {
		e.ReceivedAt = time.Now()
		select {
		case out <- e:
			return true
		case <-ctx.Done():
			return false
		}
	}
	delay := time.Second
	for ctx.Err() == nil {
		dialer := websocket.Dialer{HandshakeTimeout: time.Duration(c.TimeoutSeconds) * time.Second}
		conn, resp, err := dialer.DialContext(ctx, socketURL(c), nil)
		if resp != nil && resp.Body != nil {
			resp.Body.Close()
		}
		if err == nil {
			if !send(StreamEvent{Type: "connected", Market: c.Market}) {
				conn.Close()
				return
			}
			done := make(chan struct{})
			go func() {
				select {
				case <-ctx.Done():
					conn.Close()
				case <-done:
				}
			}()
			conn.SetReadLimit(4 << 20)
			conn.SetReadDeadline(time.Now().Add(45 * time.Second))
			conn.SetPingHandler(func(s string) error {
				conn.SetReadDeadline(time.Now().Add(45 * time.Second))
				if !send(StreamEvent{Type: "heartbeat", Market: c.Market}) {
					return ctx.Err()
				}
				return conn.WriteControl(websocket.PongMessage, []byte(s), time.Now().Add(time.Second))
			})
			for ctx.Err() == nil {
				var e StreamEvent
				if conn.ReadJSON(&e) != nil {
					break
				}
				conn.SetReadDeadline(time.Now().Add(45 * time.Second))
				if e.Market != c.Market {
					continue
				}
				if e.Type == "depth_update" {
					delay = time.Second
				}
				if !send(e) {
					break
				}
			}
			close(done)
			conn.Close()
		}
		if !send(StreamEvent{Type: "disconnected", Market: c.Market}) {
			return
		}
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
		if delay < 8*time.Second {
			delay *= 2
		}
	}
}

func (e *ArbitrageEngine) updateBook(depth Depth, receivedAt time.Time) error {
	// A cached snapshot may predate the REST bootstrap. Never roll back.
	if e.book.Ready && depth.Sequence < e.depth.Sequence {
		return nil
	}
	if err := e.book.Update(depth, receivedAt); err != nil {
		return err
	}
	e.depth = depth
	return nil
}

func (e *ArbitrageEngine) bootstrapBook(ctx context.Context) {
	var depth Depth
	if err := e.api.call(ctx, "order/orderbook", map[string]string{"marketID": e.cfg.Market}, &depth); err != nil {
		return
	}
	if err := e.updateBook(depth, time.Now()); err != nil {
		e.book.Ready = false
	}
}

func (e *ArbitrageEngine) drainMarketEvents() error {
	for {
		select {
		case event := <-e.events:
			if err := e.marketEvent(event); err != nil {
				return err
			}
		default:
			return nil
		}
	}
}

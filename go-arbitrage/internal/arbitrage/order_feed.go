package arbitrage

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	log "github.com/sirupsen/logrus"
)

type orderFeed struct {
	mu              sync.RWMutex
	market, address string
	orders          []Order
	ready           bool
	version, block  uint64
	contact         time.Time
	maxAge          time.Duration
	updates         chan struct{}
}

func newOrderFeed(c ArbitrageConfig, address string) *orderFeed {
	return &orderFeed{market: c.Market, address: address, maxAge: time.Duration(c.MaxDataAgeSeconds) * time.Second, updates: make(chan struct{}, 1)}
}

func (f *orderFeed) notify() {
	select {
	case f.updates <- struct{}{}:
	default:
	}
}

func (f *orderFeed) invalidate() {
	f.mu.Lock()
	f.ready, f.version, f.orders = false, 0, nil
	f.mu.Unlock()
	f.notify()
}

func (f *orderFeed) touch() error {
	f.mu.Lock()
	if f.ready && time.Since(f.contact) > f.maxAge {
		f.mu.Unlock()
		f.invalidate()
		return fmt.Errorf("account order heartbeat gap requires a new snapshot")
	}
	f.contact = time.Now()
	f.mu.Unlock()
	return nil
}

func (f *orderFeed) apply(raw json.RawMessage) (err error) {
	defer func() {
		if err != nil {
			f.invalidate()
		}
	}()
	var data struct {
		Address string   `json:"address"`
		Version uint64   `json:"version"`
		Block   uint64   `json:"blockNumber"`
		Open    *[]Order `json:"openOrder"`
	}
	if json.Unmarshal(raw, &data) != nil || data.Open == nil || !strings.EqualFold(data.Address, f.address) || data.Version == 0 {
		return fmt.Errorf("invalid account order snapshot")
	}
	orders, err := validateOpenOrders(*data.Open, f.market, f.address)
	if err != nil {
		return err
	}
	f.mu.Lock()
	if data.Version <= f.version {
		f.mu.Unlock()
		return fmt.Errorf("out-of-order account snapshot")
	}
	f.ready, f.version, f.orders, f.contact = true, data.Version, append([]Order{}, orders...), time.Now()
	if data.Block > f.block {
		f.block = data.Block
	}
	f.mu.Unlock()
	f.notify()
	return nil
}

func (f *orderFeed) snapshot() ([]Order, uint64, error) {
	f.mu.RLock()
	defer f.mu.RUnlock()
	if !f.ready || time.Since(f.contact) > f.maxAge {
		return nil, f.block, fmt.Errorf("account order stream is unavailable or stale")
	}
	return append([]Order{}, f.orders...), f.block, nil
}

func watchAccountOrders(ctx context.Context, c ArbitrageConfig, f *orderFeed) {
	u, _ := url.Parse(strings.TrimRight(c.Endpoint, "/") + "/v1/orders/ws")
	if u.Scheme == "https" {
		u.Scheme = "wss"
	} else {
		u.Scheme = "ws"
	}
	q := u.Query()
	q.Set("marketID", c.Market)
	q.Set("address", f.address)
	q.Set("openOnly", "1")
	u.RawQuery = q.Encode()
	defer f.invalidate()
	delay := time.Second
	for ctx.Err() == nil {
		conn, resp, err := (&websocket.Dialer{HandshakeTimeout: time.Duration(c.TimeoutSeconds) * time.Second}).DialContext(ctx, u.String(), nil)
		if resp != nil && resp.Body != nil {
			resp.Body.Close()
		}
		if err == nil {
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
				if err := f.touch(); err != nil {
					return err
				}
				conn.SetReadDeadline(time.Now().Add(45 * time.Second))
				return conn.WriteControl(websocket.PongMessage, []byte(s), time.Now().Add(time.Second))
			})
			for ctx.Err() == nil {
				var event StreamEvent
				if conn.ReadJSON(&event) != nil {
					break
				}
				if event.Type != "orders_snapshot" || event.Market != c.Market {
					break
				}
				if err := f.apply(event.Data); err != nil {
					log.Warnf("account order stream reset market=%s account=%s reason=%v", c.Market, f.address, err)
					break
				}
				conn.SetReadDeadline(time.Now().Add(45 * time.Second))
				delay = time.Second
			}
			close(done)
			conn.Close()
		}
		f.invalidate()
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

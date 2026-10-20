package arbitrage

import (
	"context"
	"math/rand"
	"time"
)

// Serialize mutations and wait 200–500 ms after each completed request.
type operationPacer struct {
	lock chan struct{}
	last time.Time
}

func newOperationPacer() *operationPacer {
	return &operationPacer{lock: make(chan struct{}, 1)}
}

func (p *operationPacer) acquire(ctx context.Context) (func(), error) {
	select {
	case p.lock <- struct{}{}:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	if !p.last.IsZero() {
		wait := time.Until(p.last.Add(time.Duration(200+rand.Intn(301)) * time.Millisecond))
		if wait > 0 {
			timer := time.NewTimer(wait)
			defer timer.Stop()
			select {
			case <-timer.C:
			case <-ctx.Done():
				<-p.lock
				return nil, ctx.Err()
			}
		}
	}
	if err := ctx.Err(); err != nil {
		<-p.lock
		return nil, err
	}
	return func() {
		p.last = time.Now()
		<-p.lock
	}, nil
}

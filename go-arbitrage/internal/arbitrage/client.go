package arbitrage

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"regexp"
	"strings"
	"time"

	log "github.com/sirupsen/logrus"
)

func readLimited(r io.Reader, limit int64) ([]byte, error) {
	b, e := io.ReadAll(io.LimitReader(r, limit+1))
	if e != nil {
		return nil, e
	}
	if int64(len(b)) > limit {
		return nil, fmt.Errorf("response exceeds size limit")
	}
	return b, nil
}

type APIError struct {
	Code   int
	Path   string
	Reason string
}

func (e *APIError) Error() string {
	if e.Reason != "" {
		return fmt.Sprintf("API %s rejected request (code=%d): %s", e.Path, e.Code, e.Reason)
	}
	return fmt.Sprintf("API %s rejected request (code=%d)", e.Path, e.Code)
}

var openOrderLimitMessage = regexp.MustCompile(`^[0-9]{1,4} open orders reached$`)

// Recognize application errors without logging arbitrary backend bodies, which
// may contain signed orders or credentials. Dynamic values are never echoed.
func safeAPIReason(desc string) string {
	if openOrderLimitMessage.MatchString(desc) {
		return desc
	}
	switch desc {
	case "invalid price", "invalid price unit", "invalid amount", "invalid amount unit",
		"invalid order signature", "order expired or not found; build a new order",
		"postOnly requires a limit order", "order already submitted or expired",
		"unable to check open order limit; please retry", "invalid open order limit configuration",
		"Market order not executed, no bid orders to be matched with",
		"Market order not executed, no ask orders to be matched with":
		return desc
	}
	if len(desc) <= 512 {
		if strings.Contains(desc, " balance not enough, available balance is ") {
			return "insufficient available account balance after locked funds"
		}
		if strings.HasPrefix(desc, "Order less then minimum size ") {
			return "order notional is below market minimum size"
		}
		if strings.HasPrefix(desc, "Amount: ") && strings.Contains(desc, " less than fee: ") {
			return "order amount is below required fee"
		}
	}
	return ""
}

type API struct {
	base  string
	http  *http.Client
	pacer *operationPacer
}

func NewAPI(c ArbitrageConfig) *API {
	return &API{
		base:  strings.TrimRight(c.Endpoint, "/") + "/",
		http:  newHTTPClient(c.TimeoutSeconds),
		pacer: newOperationPacer(),
	}
}

func newHTTPClient(timeoutSeconds int) *http.Client {
	return &http.Client{
		Timeout:       time.Duration(timeoutSeconds) * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
}

func (a *API) call(ctx context.Context, path string, request, response interface{}) (err error) {
	if a.pacer != nil && (path == "order/place" || path == "order/cancel") {
		release, err := a.pacer.acquire(ctx)
		if err != nil {
			return fmt.Errorf("API %s canceled while awaiting operation interval", path)
		}
		defer release()
	}
	if path == "order/build" || path == "order/place" || path == "order/cancel" {
		started := time.Now()
		defer func() {
			result := "ok"
			if err != nil {
				result = "error"
			}
			log.Infof("bot request path=%s result=%s duration_ms=%d", path, result, time.Since(started).Milliseconds())
		}()
	}
	body, e := json.Marshal(request)
	if e != nil {
		return fmt.Errorf("encode %s", path)
	}
	req, e := http.NewRequestWithContext(ctx, http.MethodPost, a.base+path, bytes.NewReader(body))
	if e != nil {
		return fmt.Errorf("invalid API request")
	}
	req.Header.Set("Content-Type", "application/json")
	r, e := a.http.Do(req)
	if e != nil {
		return fmt.Errorf("API %s unavailable", path)
	}
	defer r.Body.Close()
	if r.StatusCode != 200 {
		return fmt.Errorf("API %s HTTP status=%d", path, r.StatusCode)
	}
	raw, e := readLimited(r.Body, 4<<20)
	if e != nil {
		return fmt.Errorf("API %s unreadable response", path)
	}
	var envelope struct {
		Code int             `json:"code"`
		Data json.RawMessage `json:"data"`
		Desc string          `json:"desc"`
	}
	if e = json.Unmarshal(raw, &envelope); e != nil {
		return fmt.Errorf("API %s invalid JSON", path)
	}
	if envelope.Code != 20000 {
		return &APIError{Code: envelope.Code, Path: path, Reason: safeAPIReason(envelope.Desc)}
	}
	if response != nil {
		if len(envelope.Data) == 0 || string(envelope.Data) == "null" {
			return fmt.Errorf("API %s missing data", path)
		}
		if e = json.Unmarshal(envelope.Data, response); e != nil {
			var fieldError *orderFieldError
			if errors.As(e, &fieldError) {
				return fmt.Errorf("API %s invalid data: %w", path, fieldError)
			}
			return fmt.Errorf("API %s invalid data", path)
		}
	}
	return nil
}

func (a *API) Place(ctx context.Context, address, id, signature string) error {
	return a.call(ctx, "order/place", map[string]string{"address": address, "orderID": id, "signature": signature}, nil)
}

func (a *API) Cancel(ctx context.Context, market, address, id string) error {
	return a.call(ctx, "order/cancel", map[string]string{"marketID": market, "address": address, "orderID": id}, nil)
}

func (a *API) Locks(ctx context.Context, address string) (map[string]string, error) {
	var r map[string]string
	e := a.call(ctx, "lockedBalance", map[string]string{"address": address}, &r)
	if r == nil && e == nil {
		e = fmt.Errorf("missing locked balances")
	}
	return r, e
}

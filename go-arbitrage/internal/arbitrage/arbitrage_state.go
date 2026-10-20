package arbitrage

import (
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"strings"
	"syscall"
)

type arbOrder struct {
	ID           string `json:"id"`
	Relayer      string `json:"relayer"`
	Quote        Quote  `json:"quote"`
	Expires      int64  `json:"expires"`
	CreatedAt    int64  `json:"createdAt"`
	Seen         bool   `json:"seen"`
	LastCancelAt int64  `json:"lastCancelAt"`
	FilledBase   string `json:"filledBaseRaw"`
}

type arbFill struct {
	ID         string `json:"id"`
	Side       string `json:"hedgeSide"`
	Base       string `json:"baseRaw"`
	QuoteDelta string `json:"quoteDeltaRaw"`
	Block      uint64 `json:"block"`
	BlockHash  string `json:"blockHash"`
}

type arbHedgeTx struct {
	Hash      string `json:"hash"`
	Nonce     uint64 `json:"nonce"`
	Deadline  uint64 `json:"deadline"`
	FillID    string `json:"fillID"`
	Bound     string `json:"boundRaw"`
	GasBudget string `json:"gasBudgetRaw"`
}

type arbDiskState struct {
	Identity   string      `json:"identity"`
	Cursor     uint64      `json:"cursor"`
	CursorHash string      `json:"cursorHash"`
	Active     *arbOrder   `json:"active"`
	Pending    []arbFill   `json:"pending"`
	Tx         *arbHedgeTx `json:"hedgeTx"`
	Committed  string      `json:"committedQuoteRaw"`
	Loss       string      `json:"cumulativeLossRaw"`
	PnL        string      `json:"estimatedPnLRaw"`
}

type arbJournal struct {
	file  string
	lock  *os.File
	state arbDiskState
}

func arbPrivateRead(file string, limit int64) ([]byte, error) {
	f, err := os.OpenFile(file, os.O_RDONLY|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return nil, fmt.Errorf("cannot open private file")
	}
	defer f.Close()
	s, err := f.Stat()
	if err != nil || !s.Mode().IsRegular() || s.Mode().Perm()&0077 != 0 || s.Sys().(*syscall.Stat_t).Uid != uint32(os.Getuid()) {
		return nil, fmt.Errorf("private file must be owned by this user with mode 600 or stricter")
	}
	return readLimited(f, limit)
}

func arbPrivateDir(dir string) error {
	if err := os.MkdirAll(dir, 0700); err != nil {
		return err
	}
	s, err := os.Lstat(dir)
	if err != nil || !s.IsDir() || s.Mode().Perm()&0077 != 0 || s.Sys().(*syscall.Stat_t).Uid != uint32(os.Getuid()) {
		return fmt.Errorf("arbitrage state directory must be owned by this user with mode 700")
	}
	return nil
}

func openArbJournal(c ArbitrageConfig, address string) (*arbJournal, error) {
	if err := arbPrivateDir(c.StateDir); err != nil {
		return nil, err
	}
	dir := filepath.Join(c.StateDir, strings.ToLower(address))
	if err := arbPrivateDir(dir); err != nil {
		return nil, err
	}
	lock, err := os.OpenFile(filepath.Join(dir, "run.lock"), os.O_RDWR|os.O_CREATE|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, err
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		return nil, fmt.Errorf("another arbitrage instance owns this wallet state")
	}
	j := &arbJournal{file: filepath.Join(dir, "state.json"), lock: lock}
	identity, _ := json.Marshal([]interface{}{c.ChainID, c.Endpoint, c.RPCURL, c.Market, strings.ToLower(address),
		strings.ToLower(c.VaultAddress), strings.ToLower(c.ExchangeAddress),
		strings.ToLower(c.BaseToken), strings.ToLower(c.QuoteToken), strings.ToLower(c.UniswapV3Pool), c.Confirmations})
	j.state = arbDiskState{Identity: string(identity), Committed: "0", Loss: "0", PnL: "0", Pending: []arbFill{}}
	if _, err := os.Lstat(j.file); os.IsNotExist(err) {
		return j, nil
	}
	raw, err := arbPrivateRead(j.file, 1<<20)
	if err == nil {
		var saved arbDiskState
		if json.Unmarshal(raw, &saved) != nil || !compatibleArbIdentity(saved.Identity, j.state.Identity) {
			err = fmt.Errorf("arbitrage state identity is invalid; do not discard unresolved orders")
		} else {
			saved.Identity = j.state.Identity
			j.state = saved
			err = j.validate()
		}
	}
	if err != nil {
		j.close()
		return nil, err
	}
	return j, nil
}

func compatibleArbIdentity(saved, current string) bool {
	if saved == current {
		return true
	}
	// Older identities include a removed deployment fingerprint at index 7.
	var oldFields, fields []json.RawMessage
	if json.Unmarshal([]byte(saved), &oldFields) != nil || json.Unmarshal([]byte(current), &fields) != nil || len(oldFields) != 12 || len(fields) != 11 {
		return false
	}
	oldFields = append(oldFields[:7], oldFields[8:]...)
	normalized, err := json.Marshal(oldFields)
	return err == nil && string(normalized) == current
}

func (j *arbJournal) validate() error {
	if !validID(j.state.CursorHash) || arbInt(j.state.Committed) == nil || arbInt(j.state.Committed).Sign() < 0 ||
		arbInt(j.state.Loss) == nil || arbInt(j.state.Loss).Sign() < 0 || arbInt(j.state.PnL) == nil || len(j.state.Pending) > 100 {
		return fmt.Errorf("invalid arbitrage recovery amounts or cursor")
	}
	if o := j.state.Active; o != nil {
		if !validID(o.ID) || !nonzeroAddress(o.Relayer) || (o.Quote.Side != "buy" && o.Quote.Side != "sell") || !o.Quote.Amount.IsPositive() || !o.Quote.Price.IsPositive() ||
			o.Expires <= 0 || o.CreatedAt <= 0 || arbInt(o.FilledBase) == nil || arbInt(o.FilledBase).Sign() < 0 {
			return fmt.Errorf("invalid active arbitrage intent")
		}
	}
	ids := map[string]bool{}
	pendingBase := new(big.Int)
	for _, f := range j.state.Pending {
		parts := strings.Split(f.ID, ":")
		if len(parts) != 2 || !validID(parts[0]) || arbInt(parts[1]) == nil || arbInt(parts[1]).Sign() < 0 || ids[f.ID] ||
			(f.Side != "buy" && f.Side != "sell") || arbInt(f.Base) == nil || arbInt(f.Base).Sign() <= 0 || arbInt(f.QuoteDelta) == nil || !validID(f.BlockHash) || f.Block == 0 || f.Block > j.state.Cursor {
			return fmt.Errorf("invalid unhedged fill")
		}
		ids[f.ID] = true
		pendingBase.Add(pendingBase, arbInt(f.Base))
	}
	if tx := j.state.Tx; tx != nil {
		if !validID(tx.Hash) || tx.Deadline == 0 || len(j.state.Pending) == 0 || tx.FillID != j.state.Pending[0].ID ||
			arbInt(tx.Bound) == nil || arbInt(tx.Bound).Sign() <= 0 || arbInt(tx.GasBudget) == nil || arbInt(tx.GasBudget).Sign() < 0 {
			return fmt.Errorf("invalid persisted hedge intent")
		}
	}
	if len(j.state.Pending) > 0 && j.state.Active == nil {
		return fmt.Errorf("unhedged fills without an active cycle")
	}
	if j.state.Active != nil && pendingBase.Cmp(arbInt(j.state.Active.FilledBase)) > 0 {
		return fmt.Errorf("pending hedge amount exceeds settled fills")
	}
	return nil
}

func (j *arbJournal) save() error {
	data, err := json.Marshal(j.state)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(j.file), ".arbitrage-*")
	if err != nil {
		return err
	}
	name := f.Name()
	defer os.Remove(name)
	if err = f.Chmod(0600); err == nil {
		_, err = f.Write(data)
	}
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err != nil {
		return err
	}
	if closeErr != nil {
		return closeErr
	}
	if err = os.Rename(name, j.file); err != nil {
		return err
	}
	dir, err := os.Open(filepath.Dir(j.file))
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}

func (j *arbJournal) close() {
	if j != nil && j.lock != nil {
		syscall.Flock(int(j.lock.Fd()), syscall.LOCK_UN)
		j.lock.Close()
	}
}

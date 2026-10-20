package arbitrage

import (
	"crypto/ecdsa"
	"fmt"

	"github.com/ethereum/go-ethereum/accounts"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/crypto"
)

func validID(id string) bool {
	b, e := hexutil.Decode(id)
	return e == nil && len(b) == 32 && common.BytesToHash(b) != (common.Hash{})
}

func signOrderID(id string, key *ecdsa.PrivateKey) (string, error) {
	if !validID(id) {
		return "", fmt.Errorf("invalid 32-byte order ID")
	}
	hash, _ := hexutil.Decode(id)
	if key == nil {
		return "", fmt.Errorf("signer is unavailable")
	}
	sig, e := crypto.Sign(accounts.TextHash(hash), key)
	if e != nil {
		return "", fmt.Errorf("order signing failed")
	}
	out := make([]byte, 96)
	out[0] = sig[64] + 27
	copy(out[32:], sig[:64])
	return hexutil.Encode(out), nil
}

func IsValidOrderSignature(address, orderID, signature string) bool {
	h, e := hexutil.Decode(orderID)
	if e != nil || len(h) != 32 || !nonzeroAddress(address) {
		return false
	}
	s, e := hexutil.Decode(signature)
	if e != nil || len(s) != 96 || (s[0] != 27 && s[0] != 28) || s[1] != 0 {
		return false
	}
	for _, b := range s[2:32] {
		if b != 0 {
			return false
		}
	}
	sig := append([]byte(nil), s[32:]...)
	sig = append(sig, s[0]-27)
	pub, e := crypto.SigToPub(accounts.TextHash(h), sig)
	return e == nil && crypto.PubkeyToAddress(*pub) == common.HexToAddress(address)
}

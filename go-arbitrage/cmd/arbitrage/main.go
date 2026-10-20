package main

import (
	"context"
	"flag"
	"fmt"
	"go-arbitrage/internal/arbitrage"
	"os"
	"os/signal"
	"syscall"
)

func main() {
	file := flag.String("config", "arbitrage.example.json", "Path to arbitrage JSON configuration")
	live := flag.Bool("live", false, "Place real NevDEX orders and Uniswap hedges")
	once := flag.Bool("once", false, "Read-only: inspect one fresh book and executable Uniswap quotes")
	check := flag.Bool("check", false, "Read-only: validate market and Uniswap deployment, then exit")
	flag.Parse()
	if flag.NArg() != 0 || *live && (*once || *check) || *once && *check {
		fmt.Fprintln(os.Stderr, "choose --live, --once or --check; default is continuous preview")
		os.Exit(1)
	}
	c, err := arbitrage.LoadArbitrageConfig(*file)
	if err == nil {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
		defer stop()
		err = arbitrage.RunArbitrage(ctx, c, *live, *once, *check)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

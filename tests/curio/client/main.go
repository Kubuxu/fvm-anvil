package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/ethereum/go-ethereum/rpc"
	"github.com/filecoin-project/curio/api"
	"github.com/filecoin-project/go-jsonrpc"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func main() {
	ctx, stop := context.WithTimeout(context.Background(), 90*time.Second)
	defer stop()
	url := os.Args[1]
	var node api.ChainStruct
	closeRPC, err := jsonrpc.NewMergeClient(ctx, url, "Filecoin", api.GetInternalStructs(&node), nil)
	must(err)
	defer closeRPC()
	name, err := node.StateNetworkName(ctx)
	must(err)
	head, err := node.ChainHead(ctx)
	must(err)
	if head == nil || len(head.Cids()) == 0 || head.MinTimestamp() == 0 {
		panic("invalid native head")
	}
	fmt.Printf("network=%s head=%s height=%d timestamp=%d\n", name, head.Key(), head.Height(), head.MinTimestamp())
	raw, err := rpc.DialContext(ctx, url)
	must(err)
	defer raw.Close()
	eth := ethclient.NewClient(raw)
	header, err := eth.HeaderByNumber(ctx, nil)
	must(err)
	if header.BaseFee == nil {
		panic("missing baseFeePerGas")
	}
	subctx, cancel := context.WithCancel(ctx)
	changes, err := node.ChainNotify(subctx)
	must(err)
	select {
	case ch := <-changes:
		if len(ch) != 1 || string(ch[0].Type) != "current" {
			panic("expected exactly one current")
		}
	case <-ctx.Done():
		panic(ctx.Err())
	}
	var snapshot string
	must(raw.CallContext(ctx, &snapshot, "evm_snapshot"))
	var ignored interface{}
	must(raw.CallContext(ctx, &ignored, "evm_mine"))
	waitType := func(kind string) {
		for {
			select {
			case batch, ok := <-changes:
				if !ok {
					panic("stream closed")
				}
				for _, change := range batch {
					if string(change.Type) == kind {
						return
					}
				}
			case <-ctx.Done():
				panic(ctx.Err())
			}
		}
	}
	waitType("apply")
	emptyBlock, err := eth.BlockByNumber(ctx, nil)
	must(err)
	if len(emptyBlock.Transactions()) != 0 {
		panic("expected an empty interval/manual block")
	}
	var reverted bool
	must(raw.CallContext(ctx, &reverted, "evm_revert", snapshot))
	if !reverted {
		panic("snapshot revert failed")
	}
	waitType("revert")
	cancel()
	select {
	case _, ok := <-changes:
		if ok {
			for range changes {
			}
		}
	case <-ctx.Done():
		panic("cancel failed")
	}
	again, done := context.WithCancel(ctx)
	defer done()
	stream, err := node.ChainNotify(again)
	must(err)
	select {
	case batch := <-stream:
		if len(batch) != 1 || string(batch[0].Type) != "current" {
			panic("reconnect missing current")
		}
	case <-ctx.Done():
		panic(ctx.Err())
	}
	for _, hash := range os.Args[2:] {
		receipt, err := eth.TransactionReceipt(ctx, common.HexToHash(hash))
		must(err)
		if receipt.Status != 1 || len(receipt.Logs) == 0 {
			panic("failed receipt or missing event logs")
		}
		block, err := eth.BlockByHash(ctx, receipt.BlockHash)
		must(err)
		if block.Number().Cmp(receipt.BlockNumber) != 0 {
			panic("receipt/header mismatch")
		}
		found := false
		for _, tx := range block.Transactions() {
			if tx.Hash() == receipt.TxHash {
				found = true
			}
		}
		if !found {
			panic("receipt transaction missing from block body")
		}
		fmt.Printf("receipt=%s block=%s logs=%d\n", hash, receipt.BlockNumber, len(receipt.Logs))
	}
	fmt.Println("native current/apply/revert/cancel/reconnect and Ethereum decoding passed")
}

// Concurrent acknowledgment accounting shared by the guest workload and Rust integration fixtures.
package main

import (
	"context"
	"fmt"
	"sync"
	"time"
)

type ack struct {
	key, val string
	rev      int64
}

func runWrites(dur time.Duration, conc, writes int, put func(context.Context, string, string) (int64, error)) ([]ack, int) {
	deadline := time.Now().Add(dur)
	var mu sync.Mutex
	var acks []ack
	failed := 0
	var wg sync.WaitGroup
	for w := 0; w < conc; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := 0; time.Now().Before(deadline) && (writes == 0 || i < writes); i++ {
				k := fmt.Sprintf("k/%02d/%06d", w, i)
				v := fmt.Sprintf("v%d", i)
				ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
				rev, err := put(ctx, k, v)
				cancel()
				mu.Lock()
				if err == nil {
					acks = append(acks, ack{k, v, rev})
				} else {
					failed++
				}
				mu.Unlock()
				if err != nil {
					time.Sleep(50 * time.Millisecond)
					if writes > 0 {
						i--
					}
				}
			}
		}(w)
	}
	wg.Wait()
	return acks, failed
}

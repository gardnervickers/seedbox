package main

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"testing"
	"time"
)

func TestFixedWritesRetryWithoutSkippingKey(t *testing.T) {
	var mu sync.Mutex
	calls := map[string]int{}
	acks, failed := runWrites(time.Second, 3, 4, func(_ context.Context, key, val string) (int64, error) {
		mu.Lock()
		defer mu.Unlock()
		calls[key]++
		if key == "k/01/000001" && calls[key] == 1 {
			return 0, errors.New("transient")
		}
		return 42, nil
	})
	if len(acks) != 12 || failed != 1 {
		t.Fatalf("acks=%d failed=%d", len(acks), failed)
	}
	for worker := 0; worker < 3; worker++ {
		for write := 0; write < 4; write++ {
			key := fmt.Sprintf("k/%02d/%06d", worker, write)
			if calls[key] < 1 {
				t.Errorf("missing key %s", key)
			}
		}
	}
	if calls["k/01/000001"] != 2 {
		t.Fatal("did not retry failed fixed key")
	}
}
func TestFixedWritesDeadlineCanRemainIncomplete(t *testing.T) {
	acks, failed := runWrites(5*time.Millisecond, 1, 2, func(context.Context, string, string) (int64, error) { return 0, errors.New("unavailable") })
	if len(acks) != 0 || failed != 1 {
		t.Fatalf("acks=%d failed=%d", len(acks), failed)
	}
}
func TestDurationModeStillUsesNewKeysAfterFailure(t *testing.T) {
	var keys []string
	runWrites(60*time.Millisecond, 1, 0, func(_ context.Context, k, v string) (int64, error) {
		keys = append(keys, k)
		return 0, errors.New("unavailable")
	})
	if len(keys) != 2 || keys[0] == keys[1] {
		t.Fatalf("duration retry keys=%v", keys)
	}
}

func TestConcurrencyKnobStartsEveryWriter(t *testing.T) {
	var mu sync.Mutex
	entered := 0
	ready := make(chan struct{})
	acks, failed := runWrites(time.Second, 3, 1, func(ctx context.Context, key, val string) (int64, error) {
		mu.Lock()
		entered++
		if entered == 3 {
			close(ready)
		}
		mu.Unlock()
		select {
		case <-ready:
			return 42, nil
		case <-ctx.Done():
			return 0, ctx.Err()
		}
	})
	if entered != 3 || len(acks) != 3 || failed != 0 {
		t.Fatalf("entered=%d acks=%d failed=%d", entered, len(acks), failed)
	}
}

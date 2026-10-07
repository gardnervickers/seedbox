// Seedbox etcd workload client: concurrent writes with an acknowledgment
// ledger, then a consistency check across members.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	clientv3 "go.etcd.io/etcd/client/v3"
	"go.uber.org/zap"
)

type ack struct {
	key, val string
	rev      int64
}

func dial(eps []string) *clientv3.Client {
	c, err := clientv3.New(clientv3.Config{
		Endpoints:   eps,
		DialTimeout: 2 * time.Second,
		Logger:      zap.NewNop(),
	})
	if err != nil {
		fmt.Println("RESULT error dial", err)
		os.Exit(3)
	}
	return c
}

func load(eps []string, dur time.Duration, conc int, writes int) ([]ack, int) {
	cli := dial(eps)
	defer cli.Close()
	return runWrites(dur, conc, writes, func(ctx context.Context, key, val string) (int64, error) {
		r, err := cli.Put(ctx, key, val)
		if err != nil {
			return 0, err
		}
		return r.Header.Revision, nil
	})
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
					fmt.Printf("DEBUG %s w%d put %s err=%v\n", time.Now().Format("15:04:05.000000"), w, k, err)
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

// settle waits until every member answers and all report the same revision.
func settle(eps []string, timeout time.Duration) (int64, error) {
	deadline := time.Now().Add(timeout)
	var last string
	for time.Now().Before(deadline) {
		revs := map[string]int64{}
		ok := true
		for _, ep := range eps {
			c := dial([]string{ep})
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			st, err := c.Status(ctx, ep)
			cancel()
			c.Close()
			if err != nil {
				ok = false
				last = fmt.Sprintf("%s: %v", ep, err)
				break
			}
			revs[ep] = st.Header.Revision
		}
		if ok {
			min, max := int64(-1), int64(-1)
			for _, r := range revs {
				if min < 0 || r < min {
					min = r
				}
				if r > max {
					max = r
				}
			}
			if min == max {
				return min, nil
			}
			last = fmt.Sprintf("revisions differ: %v", revs)
		}
		time.Sleep(200 * time.Millisecond)
	}
	return 0, fmt.Errorf("cluster did not settle: %s", last)
}

// missingPerMember reports, for each member's own latest state, how many
// acknowledged writes it lacks.
func missingPerMember(eps []string, acks []ack) string {
	var out []string
	for _, ep := range eps {
		c := dial([]string{ep})
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		g, err := c.Get(ctx, "k/", clientv3.WithPrefix(), clientv3.WithSerializable())
		cancel()
		c.Close()
		if err != nil {
			out = append(out, fmt.Sprintf("%s: %v", ep, err))
			continue
		}
		have := make(map[string]string, len(g.Kvs))
		for _, kv := range g.Kvs {
			have[string(kv.Key)] = string(kv.Value)
		}
		missing := 0
		var first string
		for _, a := range acks {
			if have[a.key] != a.val {
				if missing == 0 {
					first = fmt.Sprintf(" first=%s@rev%d", a.key, a.rev)
				}
				missing++
			}
		}
		out = append(out, fmt.Sprintf("%s rev=%d missing=%d%s", ep, g.Header.Revision, missing, first))
	}
	return strings.Join(out, "; ")
}

func check(eps []string, acks []ack) (bool, string) {
	rev, err := settle(eps, 30*time.Second)
	if err != nil {
		return false, err.Error() + " | " + missingPerMember(eps, acks)
	}
	hashes := map[string]uint32{}
	for _, ep := range eps {
		c := dial([]string{ep})
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		h, err := c.HashKV(ctx, ep, rev)
		cancel()
		if err != nil {
			c.Close()
			return false, fmt.Sprintf("hashkv %s: %v", ep, err)
		}
		hashes[ep] = h.Hash
		ctx, cancel = context.WithTimeout(context.Background(), 10*time.Second)
		g, err := c.Get(ctx, "k/", clientv3.WithPrefix(), clientv3.WithSerializable(), clientv3.WithRev(rev))
		cancel()
		c.Close()
		if err != nil {
			return false, fmt.Sprintf("range %s: %v", ep, err)
		}
		have := make(map[string]string, len(g.Kvs))
		for _, kv := range g.Kvs {
			have[string(kv.Key)] = string(kv.Value)
		}
		missing := 0
		var first string
		for _, a := range acks {
			if have[a.key] != a.val {
				if missing == 0 {
					first = fmt.Sprintf("%s@%d", a.key, a.rev)
				}
				missing++
			}
		}
		if missing > 0 {
			return false, fmt.Sprintf("member %s missing %d acknowledged writes (first %s) rev=%d hashes=%v", ep, missing, first, rev, hashes)
		}
	}
	distinct := map[uint32]bool{}
	for _, h := range hashes {
		distinct[h] = true
	}
	if len(distinct) > 1 {
		return false, fmt.Sprintf("hashkv mismatch at rev %d: %v", rev, hashes)
	}
	return true, fmt.Sprintf("rev=%d hash=%x", rev, hashes[eps[0]])
}

func main() {
	endpoints := flag.String("endpoints", "127.0.0.1:2379", "comma-separated endpoints")
	duration := flag.Duration("duration", 5*time.Second, "write phase length")
	writes := flag.Int("writes", 0, "successful writes per worker; zero uses duration")
	concurrency := flag.Int("concurrency", 8, "concurrent writers")
	plant := flag.String("plant", "", "checker sanity control: \"missing\" adds a never-written acknowledgment")
	flag.Parse()
	if *writes < 0 || *concurrency < 1 || *duration <= 0 {
		fmt.Println("RESULT error invalid workload parameters")
		os.Exit(2)
	}
	eps := strings.Split(*endpoints, ",")
	sort.Strings(eps)
	start := time.Now()
	acks, failed := load(eps, *duration, *concurrency, *writes)
	fmt.Printf("LOAD acked=%d failed=%d elapsed=%v\n", len(acks), failed, time.Since(start))
	if *writes > 0 && len(acks) != *writes**concurrency {
		for _, ep := range eps {
			c := dial([]string{ep})
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			st, err := c.Status(ctx, ep)
			cancel()
			c.Close()
			if err != nil {
				fmt.Printf("DEBUG status %s err=%v\n", ep, err)
				continue
			}
			fmt.Printf("DEBUG status %s id=%x leader=%x term=%d index=%d applied=%d errors=%v\n", ep, st.Header.MemberId, st.Leader, st.RaftTerm, st.RaftIndex, st.RaftAppliedIndex, st.Errors)
		}
		fmt.Printf("RESULT incomplete acked=%d target=%d\n", len(acks), *writes**concurrency)
		os.Exit(4)
	}
	if *plant == "missing" {
		acks = append(acks, ack{"k/99/000000", "v0", 0})
		fmt.Println("PLANT missing acknowledgment k/99/000000")
	}
	ok, detail := check(eps, acks)
	if ok {
		fmt.Printf("RESULT pass acked=%d failed=%d %s\n", len(acks), failed, detail)
		os.Exit(0)
	}
	fmt.Printf("RESULT fail acked=%d failed=%d %s\n", len(acks), failed, detail)
	os.Exit(1)
}

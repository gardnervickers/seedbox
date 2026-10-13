// Go runtime probes: asynchronous preemption (sysmon's SIGURG and the context it rewrites),
// goroutine stack growth, copying and shrinking, and the concurrent collector's write barriers.
//
// Usage: gofz preempt|stack|gc SEED SCALE
//
// Every concurrent result is recomputed in one goroutine and must agree; the output is
// independent of scheduling, addresses and timing.
package main

import (
	"fmt"
	"hash/fnv"
	"os"
	"os/signal"
	"runtime"
	"runtime/debug"
	"sort"
	"strconv"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

var digest = fnv.New64a()

func emit(key string, v any) {
	line := fmt.Sprintf("%s %v", key, v)
	fmt.Println(line)
	digest.Write([]byte(line + "\n"))
}

func fail(format string, a ...any) {
	fmt.Printf("FAIL "+format+"\n", a...)
	os.Exit(1)
}

func xorshift(x uint64) uint64 {
	x ^= x << 13
	x ^= x >> 7
	x ^= x << 17
	return x
}

func hash64(v []uint64) string {
	h := fnv.New64a()
	for _, x := range v {
		h.Write([]byte(strconv.FormatUint(x, 16) + ","))
	}
	return fmt.Sprintf("%016x", h.Sum64())
}

func same(what string, got, want []uint64) {
	for i := range want {
		if got[i] != want[i] {
			fail("%s[%d] %x, want %x", what, i, got[i], want[i])
		}
	}
}

var sink atomic.Uint64

// ---------------------------------------------------------------------------------------------
// preempt

// work is a loop without calls, so only an asynchronous preemption stops it. Integer and
// floating-point state stays live in registers across every preemption.
//
//go:noinline
func work(seed uint64, n int) uint64 {
	x := seed | 1
	f := float64(seed%1000) + 0.5
	g := 1.0
	var acc uint64
	for i := 0; i < n; i++ {
		x ^= x << 13
		x ^= x >> 7
		x ^= x << 17
		f = f*1.0000001 + float64(x&0xff)*1e-3
		if f > 1e6 {
			f -= 1e6
		}
		g = g*0.999 + f*1e-6
		acc += x ^ uint64(int64(f*1024)) ^ uint64(int64(g*1e6))
	}
	return acc
}

// ring passes a token among n goroutines. Each spins without calls until the token is its own,
// so with one P every hand-off needs sysmon to preempt the spinner.
func ring(n, rounds, iters int, seed uint64) []uint64 {
	var token atomic.Int64
	res := make([]uint64, n)
	var wg sync.WaitGroup
	for g := 0; g < n; g++ {
		wg.Add(1)
		go func(me int) {
			defer wg.Done()
			acc := seed + uint64(me)
			spin := uint64(1)
			for r := 0; r < rounds; r++ {
				for token.Load() != int64(r*n+me) {
					spin = spin*6364136223846793005 + 1442695040888963407
				}
				acc = work(acc, iters)
				token.Store(int64(r*n + me + 1))
			}
			res[me] = acc
			sink.Add(spin)
		}(g)
	}
	wg.Wait()
	return res
}

func ringWant(n, rounds, iters int, seed uint64) []uint64 {
	res := make([]uint64, n)
	for me := range res {
		acc := seed + uint64(me)
		for r := 0; r < rounds; r++ {
			acc = work(acc, iters)
		}
		res[me] = acc
	}
	return res
}

// spinners starts n goroutines that spin without calls until stop is set.
func spinners(n int, stop *atomic.Bool, wg *sync.WaitGroup) {
	var started atomic.Int64
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			started.Add(1)
			x := uint64(i + 1)
			for !stop.Load() {
				x = x*3 + 1
			}
			sink.Add(x)
		}(i)
	}
	for started.Load() < int64(n) {
		runtime.Gosched()
	}
}

// spinGC runs collections and other stop-the-world operations while goroutines spin.
func spinGC(spin, gcs int) {
	var stop atomic.Bool
	var wg sync.WaitGroup
	spinners(spin, &stop, &wg)
	keep := make([][]byte, 0, gcs)
	for k := 0; k < gcs; k++ {
		keep = append(keep, make([]byte, 1<<20))
		runtime.GC()
		var ms runtime.MemStats
		runtime.ReadMemStats(&ms)
	}
	stop.Store(true)
	wg.Wait()
	sink.Add(uint64(len(keep)))
}

// signals delivers SIGUSR1 to the process one at a time while goroutines spin.
func signals(spin, count int) int {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGUSR1)
	defer signal.Stop(ch)
	var stop atomic.Bool
	var wg sync.WaitGroup
	spinners(spin, &stop, &wg)
	got := 0
	for k := 0; k < count; k++ {
		if err := syscall.Kill(os.Getpid(), syscall.SIGUSR1); err != nil {
			fail("kill: %v", err)
		}
		<-ch
		got++
	}
	stop.Store(true)
	wg.Wait()
	return got
}

// sliced runs independent loops time-sliced on the available Ps.
func sliced(n, iters int, seed uint64) []uint64 {
	res := make([]uint64, n)
	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			res[i] = work(seed+uint64(i)*7919, iters)
		}(i)
	}
	wg.Wait()
	return res
}

func preempt(seed uint64, scale int) {
	for _, procs := range []int{1, 3} {
		runtime.GOMAXPROCS(procs)
		n, rounds, iters := 4, 4*scale, 3_000_000
		got := ring(n, rounds, iters, seed)
		same(fmt.Sprintf("ring.p%d", procs), got, ringWant(n, rounds, iters, seed))
		emit(fmt.Sprintf("ring.p%d", procs), hash64(got))

		got = sliced(8, 2_000_000, seed)
		want := make([]uint64, 8)
		for i := range want {
			want[i] = work(seed+uint64(i)*7919, 2_000_000)
		}
		same(fmt.Sprintf("sliced.p%d", procs), got, want)
		emit(fmt.Sprintf("sliced.p%d", procs), hash64(got))

		spinGC(3, 4*scale)
		emit(fmt.Sprintf("spingc.p%d", procs), "ok")
		emit(fmt.Sprintf("signals.p%d", procs), signals(3, 10*scale))
	}
	runtime.GOMAXPROCS(1)
}

// ---------------------------------------------------------------------------------------------
// stack

// deep recurses with a 320-byte frame and passes a pointer into its caller's frame, so every
// stack copy must adjust it.
//
//go:noinline
func deep(n int, p *uint64, r uint64, bottom func()) uint64 {
	var local [40]uint64
	local[n%40] = r ^ *p
	local[(n+7)%40] = uint64(n)
	if n == 0 {
		bottom()
		return local[0] + *p
	}
	v := deep(n-1, &local[n%40], r*0x9E3779B97F4A7C15+uint64(n), bottom)
	return (v ^ local[n%40]) + local[(n+7)%40] + *p
}

var defers atomic.Int64

//go:noinline
func deepPanic(n int, r uint64) uint64 {
	var local [24]uint64
	local[n%24] = r
	defer func() { defers.Add(1) }()
	if n == 0 {
		panic(r)
	}
	return deepPanic(n-1, r*31+uint64(n)) + local[n%24]
}

func catch(n int, r uint64) (v uint64) {
	defer func() {
		if e := recover(); e != nil {
			v = e.(uint64)
		}
	}()
	return deepPanic(n, r)
}

func stack(seed uint64, scale int) {
	runtime.GOMAXPROCS(2)
	const G = 64
	x := seed | 1
	var all []uint64
	for round := 0; round < 8*scale; round++ {
		depths := make([]int, G)
		seeds := make([]uint64, G)
		for i := range depths {
			x = xorshift(x)
			depths[i] = 50 + int(x%1500)
			seeds[i] = x
		}
		want := make([]uint64, G)
		for i := range want {
			base := uint64(i)
			want[i] = deep(depths[i], &base, seeds[i], func() {})
		}
		got := make([]uint64, G)
		var arrived, wg sync.WaitGroup
		arrived.Add(G)
		gate := make(chan struct{})
		var stop atomic.Bool
		var gcs sync.WaitGroup
		gcs.Add(1)
		go func() { // collections while the stacks grow
			defer gcs.Done()
			for !stop.Load() {
				runtime.GC()
				time.Sleep(time.Millisecond)
			}
		}()
		for i := 0; i < G; i++ {
			wg.Add(1)
			go func(i int) {
				defer wg.Done()
				base := uint64(i)
				got[i] = deep(depths[i], &base, seeds[i], func() { arrived.Done(); <-gate })
			}(i)
		}
		arrived.Wait() // every goroutine is at its deepest point
		runtime.GC()
		close(gate)
		wg.Wait()
		stop.Store(true)
		gcs.Wait()
		runtime.GC() // shrinks the now idle stacks
		same(fmt.Sprintf("deep round %d", round), got, want)
		all = append(all, got...)
	}
	emit("deep", hash64(all))

	var pv []uint64
	for i := 0; i < 1000*scale; i++ {
		x = xorshift(x)
		pv = append(pv, catch(int(x%800), x))
	}
	emit("panics", hash64(pv))
	emit("defers", defers.Load())

	// Goroutines that grow, block and grow again, each with a pointer into its own stack.
	var wg sync.WaitGroup
	res := make([]uint64, 256)
	for i := range res {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			var acc uint64
			base := uint64(i)
			for k := 0; k < 4; k++ {
				acc += deep(100+(i*37+k*101)%700, &base, uint64(i*k), runtime.Gosched)
			}
			res[i] = acc
		}(i)
	}
	wg.Wait()
	want := make([]uint64, len(res))
	for i := range want {
		base := uint64(i)
		for k := 0; k < 4; k++ {
			want[i] += deep(100+(i*37+k*101)%700, &base, uint64(i*k), func() {})
		}
	}
	same("regrow", res, want)
	emit("regrow", hash64(res))
}

// ---------------------------------------------------------------------------------------------
// gc

type node struct {
	next *node
	val  uint64
	blob []byte
}

// mutate runs a seeded sequence of list operations. Every pointer store hits the write
// barrier while a collection is marking.
func mutate(seed uint64, ops int) uint64 {
	const L = 64
	lists := make([]*node, L)
	index := make(map[uint64]*node)
	x := seed | 1
	for i := 0; i < ops; i++ {
		x = xorshift(x)
		j, k := int(x%L), int((x>>8)%L)
		switch (x >> 16) % 8 {
		case 0, 1, 2: // push
			n := &node{next: lists[j], val: x}
			if (x>>24)%16 == 0 {
				n.blob = make([]byte, 1+int((x>>32)%40000))
				n.blob[0], n.blob[len(n.blob)-1] = byte(x), byte(x>>8)
			}
			lists[j] = n
			if (x>>28)%4 == 0 {
				index[x%4096] = n
			}
		case 3, 4: // move the head of list j to list k
			if n := lists[j]; n != nil {
				lists[j] = n.next
				n.next = lists[k]
				lists[k] = n
			}
		case 5: // drop the head
			if n := lists[j]; n != nil {
				lists[j] = n.next
			}
		case 6: // rewrite values in place
			for n := lists[j]; n != nil; n = n.next {
				n.val = n.val*31 + 7
			}
		case 7: // truncate at depth 8
			n := lists[j]
			for d := 0; n != nil && d < 8; d++ {
				n = n.next
			}
			if n != nil {
				n.next = nil
			}
		}
	}
	h := fnv.New64a()
	put := func(v uint64) { h.Write([]byte(strconv.FormatUint(v, 16) + ",")) }
	for _, l := range lists {
		for n := l; n != nil; n = n.next {
			put(n.val)
			if n.blob != nil {
				put(uint64(len(n.blob))<<16 | uint64(n.blob[0])<<8 | uint64(n.blob[len(n.blob)-1]))
			}
		}
		put(0)
	}
	keys := make([]uint64, 0, len(index))
	for k := range index {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(a, b int) bool { return keys[a] < keys[b] })
	for _, k := range keys {
		put(k)
		put(index[k].val)
	}
	return h.Sum64()
}

type finalized struct {
	p   *int
	pad [4]uint64
}

//go:noinline
func garbage(n int, count *atomic.Int64) {
	for i := 0; i < n; i++ {
		v := i
		o := &finalized{p: &v}
		runtime.SetFinalizer(o, func(*finalized) { count.Add(1) })
	}
}

func gc(seed uint64, scale int) {
	runtime.GOMAXPROCS(2)
	debug.SetGCPercent(10)
	const W = 4
	ops := 150_000 * scale
	want := make([]uint64, W)
	for i := range want {
		want[i] = mutate(seed+uint64(i), ops)
	}
	got := make([]uint64, W)
	var wg sync.WaitGroup
	for i := range got {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			got[i] = mutate(seed+uint64(i), ops)
		}(i)
	}
	wg.Wait()
	same("mutate", got, want)
	emit("mutate", hash64(got))

	var count atomic.Int64
	n := 2000 * scale
	garbage(n, &count)
	for i := 0; i < 2000 && count.Load() < int64(n); i++ {
		runtime.GC()
		time.Sleep(time.Millisecond)
	}
	emit("finalized", count.Load())
	debug.SetGCPercent(100)
}

func main() {
	if len(os.Args) != 4 {
		fmt.Println("FAIL usage: gofz preempt|stack|gc SEED SCALE")
		os.Exit(2)
	}
	seed, err1 := strconv.ParseUint(os.Args[2], 10, 64)
	scale, err2 := strconv.Atoi(os.Args[3])
	if err1 != nil || err2 != nil || scale < 1 {
		fail("bad SEED or SCALE")
	}
	tests := map[string]func(uint64, int){"preempt": preempt, "stack": stack, "gc": gc}
	test, ok := tests[os.Args[1]]
	if !ok {
		fail("unknown test %s", os.Args[1])
	}
	test(seed, scale)
	fmt.Printf("OK Go.%s %016x\n", os.Args[1], digest.Sum64())
}

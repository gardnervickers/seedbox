//go:build cgo

// cgoThreads: pthreads created in C call exported Go functions (cgo callbacks on threads the Go
// runtime did not create, each bound to an extra M), which recurse with large frames so the
// callback goroutine's stack grows and moves, keep pointers into their own frames, allocate,
// call C again, and from C call Go once more; goroutines collect concurrently. Built only with
// CGO_ENABLED=1 (/fuzz/bin/gofzcgo).
package main

/*
#include <pthread.h>
#include <stdint.h>

extern uint64_t goVisit(uint64_t seed, int depth, int thread);
extern uint64_t goLeaf(uint64_t x);

static uint64_t cmix(uint64_t h, uint64_t x) {
	h ^= x;
	h *= 0x100000001B3ULL;
	return h ^ (h >> 29);
}

// C called from a callback; every eighth call goes back into Go (a nested callback).
static uint64_t cfz_leaf(uint64_t x, int depth) {
	volatile uint64_t frame[32];
	for (int i = 0; i < 32; i++) frame[i] = x + (uint64_t)i * 0x9E3779B97F4A7C15ULL;
	uint64_t h = 0xcbf29ce484222325ULL;
	for (int i = 0; i < 32; i++) h = cmix(h, frame[i] ^ (uint64_t)depth);
	if ((x & 7) == 3) h = cmix(h, goLeaf(h));
	return h;
}

struct cfz_arg {
	int index, calls;
	uint64_t seed, result;
};

static void *cfz_thread(void *p) {
	struct cfz_arg *a = p;
	uint64_t acc = 0;
	for (int k = 0; k < a->calls; k++)
		acc = cmix(acc, goVisit(a->seed + (uint64_t)k * 7919, 16 + (int)((a->seed + (uint64_t)k) % 160), a->index));
	a->result = acc;
	return 0;
}

// Runs `threads` pthreads (at most 32) of `calls` callbacks each; inline != 0 runs the same
// loops on the calling thread instead, one after another.
static uint64_t cfz_run(int threads, int calls, uint64_t seed, int inl) {
	pthread_t t[32];
	struct cfz_arg a[32];
	if (threads > 32) threads = 32;
	for (int i = 0; i < threads; i++) {
		a[i].index = i;
		a[i].calls = calls;
		a[i].seed = seed + (uint64_t)i * 1000003;
		a[i].result = 0;
		if (inl) cfz_thread(&a[i]);
		else if (pthread_create(&t[i], 0, cfz_thread, &a[i]) != 0) return 0;
	}
	uint64_t acc = 0;
	for (int i = 0; i < threads; i++) {
		if (!inl) pthread_join(t[i], 0);
		acc = cmix(acc, a[i].result);
	}
	return acc;
}
*/
import "C"

import (
	"fmt"
	"runtime"
	"sync"
	"sync/atomic"
)

var callbacks, leaves atomic.Int64

// visitDeep recurses with 512-byte frames and a pointer into the caller's frame, so stack copies
// must adjust it; at the bottom it allocates and calls C.
//
//go:noinline
func visitDeep(n int, p *uint64, x uint64) uint64 {
	var local [64]uint64
	local[n%64] = x ^ *p
	local[(n+13)%64] = uint64(n)
	if n == 0 {
		keep := make([]uint64, 16+int(x%200))
		for i := range keep {
			keep[i] = x + uint64(i)
		}
		v := uint64(C.cfz_leaf(C.uint64_t(x), C.int(len(keep))))
		return v ^ keep[len(keep)-1] ^ local[0]
	}
	v := visitDeep(n-1, &local[n%64], x*0x9E3779B97F4A7C15+uint64(n))
	return (v ^ local[n%64]) + local[(n+13)%64] + *p
}

//export goVisit
func goVisit(seed C.uint64_t, depth C.int, thread C.int) C.uint64_t {
	callbacks.Add(1)
	base := uint64(thread) + 1
	v := visitDeep(int(depth), &base, uint64(seed))
	m := make(map[uint64]uint64, 8)
	for i := uint64(0); i < 8; i++ {
		m[v%97+i] = v ^ i
	}
	var s uint64
	for k, w := range m {
		s += k ^ w
	}
	return C.uint64_t(v + s)
}

//export goLeaf
func goLeaf(x C.uint64_t) C.uint64_t {
	leaves.Add(1)
	b := make([]byte, 64+int(x%256))
	for i := range b {
		b[i] = byte(uint64(x) >> (i % 8 * 8))
	}
	var h uint64 = 14695981039346656037
	for _, c := range b {
		h = (h ^ uint64(c)) * 1099511628211
	}
	return C.uint64_t(h)
}

func cgoThreads(seed uint64, scale int) {
	runtime.GOMAXPROCS(2)
	var stop atomic.Bool
	var wg sync.WaitGroup
	// Collections and allocation while callbacks run on foreign threads.
	for g := 0; g < 2; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			var keep [][]byte
			for i := 0; !stop.Load(); i++ {
				keep = append(keep, make([]byte, 16<<10))
				if len(keep) > 32 {
					keep = keep[:0]
				}
				if g == 0 && i%16 == 0 {
					runtime.GC()
				}
				runtime.Gosched()
			}
			sink.Add(uint64(len(keep)))
		}(g)
	}
	batches := 30 * scale
	var all []uint64
	for b := 0; b < batches; b++ {
		threads, calls := 2+b%7, 6+b%5
		s := seed*0x2545F4914F6CDD1D + uint64(b)
		want := uint64(C.cfz_run(C.int(threads), C.int(calls), C.uint64_t(s), 1))
		got := uint64(C.cfz_run(C.int(threads), C.int(calls), C.uint64_t(s), 0))
		if got != want {
			fail("cgo batch %d: threads %x, inline %x", b, got, want)
		}
		all = append(all, got)
	}
	// Goroutines calling into C concurrently with the foreign threads' callbacks.
	res := make([]uint64, 4)
	var cw sync.WaitGroup
	for g := range res {
		cw.Add(1)
		go func(g int) {
			defer cw.Done()
			res[g] = uint64(C.cfz_run(C.int(3), C.int(4), C.uint64_t(seed+uint64(g)*77), 0))
		}(g)
	}
	cw.Wait()
	for g := range res {
		if want := uint64(C.cfz_run(C.int(3), C.int(4), C.uint64_t(seed+uint64(g)*77), 1)); res[g] != want {
			fail("cgo goroutine %d: %x, want %x", g, res[g], want)
		}
	}
	stop.Store(true)
	wg.Wait()
	emit("cgo.batches", fmt.Sprintf("%d %s", batches, hash64(all)))
	emit("cgo.goroutines", hash64(res))
	emit("cgo.callbacks", callbacks.Load())
	emit("cgo.leaves", leaves.Load())
}

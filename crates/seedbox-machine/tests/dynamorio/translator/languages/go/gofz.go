// Go runtime probes: asynchronous preemption (sysmon's SIGURG and the context it rewrites),
// goroutine stack growth, copying and shrinking, the concurrent collector's write barriers,
// child processes started through Go's own vfork-style clone (exec), memory faults turned into
// recoverable panics (faultRecovery) and, in the cgo build, callbacks from C threads
// (cgoThreads, cgo.go).
//
// Usage: gofz preempt|stack|gc|exec|faultRecovery|cgoThreads SEED SCALE
//
// Every concurrent result is recomputed in one goroutine and must agree; the output is
// independent of scheduling, addresses and timing.
package main

import (
	"bufio"
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"runtime/debug"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unsafe"
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
				runtime.Gosched()
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
	for count.Load() < int64(n) {
		runtime.GC()
		runtime.Gosched()
	}
	emit("finalized", count.Load())
	debug.SetGCPercent(100)
}

// ---------------------------------------------------------------------------------------------
// exec

// Go's forkExec clones with CLONE_VFORK|CLONE_VM through its own raw syscall path, not libc's.
// The children are this executable again ("gofz exec-child MODE ..."). Background goroutines
// allocate, collect and spin (so sysmon preempts them) throughout.

const execMark = "GOFZ_EXEC_MARK"

func self() string {
	if p, err := os.Executable(); err == nil {
		return p
	}
	return os.Args[0]
}

// execNoise allocates, collects and spins until stop is set.
func execNoise(stop *atomic.Bool, wg *sync.WaitGroup) {
	wg.Add(1)
	go func() {
		defer wg.Done()
		var keep [][]byte
		for i := 0; !stop.Load(); i++ {
			keep = append(keep, make([]byte, 32<<10))
			if len(keep) > 64 {
				keep = keep[:0]
			}
			if i%128 == 127 {
				runtime.GC()
			}
			runtime.Gosched()
		}
		sink.Add(uint64(len(keep)))
	}()
	spinners(2, stop, wg)
}

func childStatus(seed uint64, i int) int {
	return []int{0, 1, 2, 3, 42}[(seed+uint64(i)*7)%5]
}

// runRecord starts one child that writes "rec I" and exits with its chosen status.
func runRecord(seed uint64, i int) string {
	want := childStatus(seed, i)
	cmd := exec.Command(self(), "exec-child", "rec", strconv.Itoa(i), strconv.Itoa(want))
	var stdout, stderr strings.Builder
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Start(); err != nil {
		fail("start rec %d: %v", i, err)
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	var err error
	select {
	case err = <-done:
	case <-time.After(120 * time.Second):
		fail("rec %d: child still running after 120 s (%s)", i, procStatus(cmd.Process.Pid))
	}
	out := stdout.String()
	code := 0
	var ee *exec.ExitError
	if errors.As(err, &ee) {
		code = ee.ExitCode()
	} else if err != nil {
		fail("exec rec %d: %v", i, err)
	}
	if code != want {
		// The wait's own view: the pid it reaped and the raw status, against the started pid.
		ps := cmd.ProcessState
		fail("rec %d exit %d, want %d (started pid %d, waited pid %d, status %#x, output %q, stderr %q)", i, code,
			want, cmd.Process.Pid, ps.Pid(), uint32(ps.Sys().(syscall.WaitStatus)), out, stderr.String())
	}
	return out
}

// records runs n record children from `workers` goroutines and checks every record once.
func records(seed uint64, n, workers int) string {
	got := make([]string, n)
	var next atomic.Int64
	var wg sync.WaitGroup
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				i := int(next.Add(1) - 1)
				if i >= n {
					return
				}
				got[i] = runRecord(seed, i)
			}
		}()
	}
	wg.Wait()
	h := fnv.New64a()
	for i, rec := range got {
		if rec != fmt.Sprintf("rec %d\n", i) {
			fail("record %d is %q", i, rec)
		}
		fmt.Fprintf(h, "%d:%d,", i, childStatus(seed, i))
	}
	return fmt.Sprintf("%016x", h.Sum64())
}

// procStatus summarizes a process's state and pending signals for a failure message.
func procStatus(pid int) string {
	b, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return err.Error()
	}
	var parts []string
	for _, line := range strings.Split(string(b), "\n") {
		for _, key := range []string{"State:", "SigPnd:", "ShdPnd:", "SigBlk:", "SigCgt:", "Threads:"} {
			if strings.HasPrefix(line, key) {
				parts = append(parts, strings.Join(strings.Fields(line), " "))
			}
		}
	}
	// Each thread's system call (number and first argument), as /proc shows it to a parent.
	if tasks, err := os.ReadDir(fmt.Sprintf("/proc/%d/task", pid)); err == nil {
		for _, t := range tasks {
			if b, err := os.ReadFile(fmt.Sprintf("/proc/%d/task/%s/syscall", pid, t.Name())); err == nil {
				f := strings.Fields(string(b))
				if len(f) > 2 {
					f = f[:2]
				}
				parts = append(parts, "task "+strings.Join(f, " "))
			}
		}
	}
	return strings.Join(parts, ", ")
}

// pacedSignals runs `children` children one at a time from a goroutine locked to its thread;
// each sends `per` SIGUSR1 to this process, waiting for an acknowledgement after each, so no
// two are ever pending together. Returns the number received.
func pacedSignals(usr1 chan os.Signal, children, per int) int {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	got := 0
	for c := 0; c < children; c++ {
		cmd := exec.Command(self(), "exec-child", "sig", strconv.Itoa(c), strconv.Itoa(per))
		stdin, err := cmd.StdinPipe()
		if err != nil {
			fail("stdin pipe: %v", err)
		}
		stdout, err := cmd.StdoutPipe()
		if err != nil {
			fail("stdout pipe: %v", err)
		}
		if err := cmd.Start(); err != nil {
			fail("start sig child: %v", err)
		}
		type result struct {
			out []byte
			err error
		}
		exited := make(chan result, 1)
		go func() {
			out, _ := io.ReadAll(stdout)
			exited <- result{out, cmd.Wait()}
		}()
		for k := 0; k < per; k++ {
			select {
			case <-usr1:
				got++
			case r := <-exited:
				fail("sig child %d exited before signal %d: %v, printed %q", c, k, r.err, r.out)
			case <-time.After(60 * time.Second):
				// Does a signal this process sends itself still arrive?
				syscall.Kill(os.Getpid(), syscall.SIGUSR1)
				late := "no"
				select {
				case <-usr1:
					late = "yes"
				case <-time.After(10 * time.Second):
				}
				fail("sig child %d: signal %d never arrived (self-signal afterwards arrives: %s; child %s; parent %s)",
					c, k, late, procStatus(cmd.Process.Pid), procStatus(os.Getpid()))
			}
			if _, err := stdin.Write([]byte{'a'}); err != nil {
				fail("ack: %v", err)
			}
		}
		stdin.Close()
		r := <-exited
		if r.err != nil {
			fail("sig child %d: %v", c, r.err)
		}
		if want := fmt.Sprintf("sig %d %d\n", c, per); string(r.out) != want {
			fail("sig child %d printed %q", c, r.out)
		}
	}
	return got
}

func execProbe(seed uint64, scale int) {
	runtime.GOMAXPROCS(2)
	var stop atomic.Bool
	var wg sync.WaitGroup
	execNoise(&stop, &wg)

	// (a) Record children from four goroutines at once.
	n := 200 * scale
	emit("exec.records", fmt.Sprintf("%d %s", n, records(seed, n, 4)))

	// (b) The same with SIGCHLD and SIGUSR1 handled through os/signal, paced SIGUSR1 from
	// children started on a locked thread, and record children running alongside.
	usr1 := make(chan os.Signal, 64)
	chld := make(chan os.Signal, 1024)
	signal.Notify(usr1, syscall.SIGUSR1)
	signal.Notify(chld, syscall.SIGCHLD)
	var side sync.WaitGroup
	side.Add(1)
	var sideHash string
	go func() {
		defer side.Done()
		sideHash = records(seed^0xabcdef, 40*scale, 2)
	}()
	children, per := 4*scale, 5
	got := pacedSignals(usr1, children, per)
	side.Wait()
	signal.Stop(usr1)
	signal.Stop(chld)
	extra := len(usr1)
	emit("exec.signals", fmt.Sprintf("%d of %d, extra %d", got, children*per, extra))
	emit("exec.sigchld", len(chld) > 0)
	emit("exec.side", sideHash)

	// (c) Children that set a marker and syscall.Exec this program again, from the main
	// goroutine or from a goroutine locked to another thread while others run.
	h := fnv.New64a()
	m := 20 * scale
	for i := 0; i < m; i++ {
		out, err := exec.Command(self(), "exec-child", "stage1", strconv.Itoa(i)).Output()
		code := 0
		var ee *exec.ExitError
		if errors.As(err, &ee) {
			code = ee.ExitCode()
		} else if err != nil {
			fail("stage1 %d: %v", i, err)
		}
		if want := fmt.Sprintf("exec %d m%d samepid\n", i, i); string(out) != want || code != i%4 {
			fail("stage1 %d printed %q exit %d", i, out, code)
		}
		fmt.Fprintf(h, "%s%d,", out, code)
	}
	emit("exec.reexec", fmt.Sprintf("%d %016x", m, h.Sum64()))

	stop.Store(true)
	wg.Wait()
	var ws syscall.WaitStatus
	pid, err := syscall.Wait4(-1, &ws, syscall.WNOHANG, nil)
	emit("exec.leaked", pid > 0 || !errors.Is(err, syscall.ECHILD))
}

// execChild is the child side: "rec I STATUS", "sig I COUNT", "stage1 I", "stage2 I PID".
func execChild(args []string) {
	if len(args) < 2 {
		os.Exit(100)
	}
	i, _ := strconv.Atoi(args[1])
	switch args[0] {
	case "rec":
		status, _ := strconv.Atoi(args[2])
		fmt.Printf("rec %d\n", i)
		os.Exit(status)
	case "sig":
		count, _ := strconv.Atoi(args[2])
		in := bufio.NewReader(os.Stdin)
		for k := 0; k < count; k++ {
			if err := syscall.Kill(os.Getppid(), syscall.SIGUSR1); err != nil {
				os.Exit(101)
			}
			if _, err := in.ReadByte(); err != nil {
				os.Exit(102)
			}
		}
		fmt.Printf("sig %d %d\n", i, count)
		os.Exit(0)
	case "stage1":
		runtime.GOMAXPROCS(2)
		var stop atomic.Bool
		var wg sync.WaitGroup
		execNoise(&stop, &wg)
		env := append(os.Environ(), fmt.Sprintf("%s=m%d", execMark, i))
		argv := []string{"gofz", "exec-child", "stage2", strconv.Itoa(i), strconv.Itoa(os.Getpid())}
		run := func() {
			err := syscall.Exec(self(), argv, env)
			fmt.Printf("exec failed: %v\n", err)
			os.Exit(103)
		}
		if i%2 == 1 {
			go func() {
				runtime.LockOSThread()
				run()
			}()
			select {}
		}
		run()
	case "stage2":
		pid, _ := strconv.Atoi(args[2])
		same := "otherpid"
		if pid == os.Getpid() {
			same = "samepid"
		}
		fmt.Printf("exec %d %s %s\n", i, os.Getenv(execMark), same)
		os.Exit(i % 4)
	}
	os.Exit(104)
}

// ---------------------------------------------------------------------------------------------
// faultRecovery

// With debug.SetPanicOnFault, a fault on a protected page becomes a runtime.Error panic carrying
// the fault address, raised from the signal frame the kernel (or a translator) built. Faults are
// taken through noinline loads and stores at several stack depths, on stacks that have just
// grown, with defers to run; each is recovered, access restored and the data checked, while
// other goroutines collect and spin so preemption and GC meet the faulting goroutines.

type addrError interface{ Addr() uintptr }

//go:noinline
func faultLoad(p *uint64) uint64 { return *p }

//go:noinline
func faultStore(p *uint64, v uint64) { *p = v }

var faultDefers atomic.Int64

// faultDeep recurses n frames of 256 bytes, then loads (or stores) through p.
//
//go:noinline
func faultDeep(n int, p *uint64, store bool, x uint64) uint64 {
	var local [32]uint64
	local[n%32] = x
	if n%5 == 0 {
		defer func() { faultDefers.Add(1) }()
	}
	if n == 0 {
		if store {
			faultStore(p, x)
			return x
		}
		return faultLoad(p) ^ local[0]
	}
	return faultDeep(n-1, p, store, x*31+uint64(n)) + local[n%32]
}

// tryFault returns the fault address (0 when no fault happened) and the value read.
func tryFault(n int, p *uint64, store bool, x uint64) (addr uintptr, v uint64) {
	defer func() {
		if e := recover(); e != nil {
			ae, ok := e.(addrError)
			if !ok {
				fail("fault panic %v is not an address error", e)
			}
			if _, ok := e.(runtime.Error); !ok {
				fail("fault panic %v is not a runtime.Error", e)
			}
			addr = ae.Addr()
		}
	}()
	return 0, faultDeep(n, p, store, x)
}

func faultWorker(id int, seed uint64, rounds int) (uint64, int) {
	debug.SetPanicOnFault(true)
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	pg := os.Getpagesize()
	const pages = 8
	mem, err := syscall.Mmap(-1, 0, pages*pg, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_ANON|syscall.MAP_PRIVATE)
	if err != nil {
		fail("mmap: %v", err)
	}
	defer syscall.Munmap(mem)
	words := pages * pg / 8
	word := func(i int) *uint64 { return (*uint64)(unsafe.Pointer(&mem[i*8])) }
	x := seed | 1
	for i := 0; i < words; i++ {
		x = xorshift(x)
		*word(i) = x
	}
	model := make([]uint64, words)
	for i := range model {
		model[i] = *word(i)
	}
	h := fnv.New64a()
	faults := 0
	for r := 0; r < rounds; r++ {
		x = xorshift(x)
		page := int(x % pages)
		i := page*(pg/8) + int((x>>8)%uint64(pg/8))
		depth := []int{0, 1, 7, 60, 300}[(x>>20)%5]
		store := (x>>24)%3 == 0
		prot := syscall.PROT_NONE
		if store && (x>>26)%2 == 0 {
			prot = syscall.PROT_READ // a store to a read-only page faults too
		}
		pageMem := mem[page*pg : (page+1)*pg]
		if err := syscall.Mprotect(pageMem, prot); err != nil {
			fail("mprotect: %v", err)
		}
		addr, _ := tryFault(depth, word(i), store, x)
		if err := syscall.Mprotect(pageMem, syscall.PROT_READ|syscall.PROT_WRITE); err != nil {
			fail("mprotect: %v", err)
		}
		if addr != uintptr(unsafe.Pointer(word(i))) {
			fail("fault %d.%d at %#x, want %#x", id, r, addr, uintptr(unsafe.Pointer(word(i))))
		}
		faults++
		// The page still holds the model; an unprotected access now succeeds.
		if a, v := tryFault(0, word(i), false, x); a != 0 || v != model[i]^x {
			fail("load after restore %d.%d: fault %#x, value %x", id, r, a, v)
		}
		if *word(i) != model[i] {
			fail("word %d changed by a faulting store: %x, want %x", i, *word(i), model[i])
		}
		fmt.Fprintf(h, "%d:%d:%t,", i, depth, store)
	}
	for i := range model {
		if *word(i) != model[i] {
			fail("word %d: %x, want %x", i, *word(i), model[i])
		}
	}
	return h.Sum64(), faults
}

func faultRecovery(seed uint64, scale int) {
	runtime.GOMAXPROCS(2)
	var stop atomic.Bool
	var wg sync.WaitGroup
	execNoise(&stop, &wg)
	const W = 4
	rounds := 400 * scale
	hs := make([]uint64, W)
	counts := make([]int, W)
	var fw sync.WaitGroup
	for w := 0; w < W; w++ {
		fw.Add(1)
		go func(w int) {
			defer fw.Done()
			hs[w], counts[w] = faultWorker(w, seed+uint64(w)*0x9E37, rounds)
		}(w)
	}
	fw.Wait()
	stop.Store(true)
	wg.Wait()
	total := 0
	for _, c := range counts {
		total += c
	}
	emit("fault.recovered", fmt.Sprintf("%d of %d", total, W*rounds))
	emit("fault.shapes", hash64(hs))
	emit("fault.defers", faultDefers.Load())
}

func main() {
	if len(os.Args) > 2 && os.Args[1] == "exec-child" {
		execChild(os.Args[2:])
	}
	if len(os.Args) != 4 {
		fmt.Println("FAIL usage: gofz preempt|stack|gc|exec|faultRecovery|cgoThreads SEED SCALE")
		os.Exit(2)
	}
	seed, err1 := strconv.ParseUint(os.Args[2], 10, 64)
	scale, err2 := strconv.Atoi(os.Args[3])
	if err1 != nil || err2 != nil || scale < 1 {
		fail("bad SEED or SCALE")
	}
	tests := map[string]func(uint64, int){"preempt": preempt, "stack": stack, "gc": gc, "exec": execProbe,
		"faultRecovery": faultRecovery, "cgoThreads": cgoThreads}
	test, ok := tests[os.Args[1]]
	if !ok {
		fail("unknown test %s", os.Args[1])
	}
	test(seed, scale)
	fmt.Printf("OK Go.%s %016x\n", os.Args[1], digest.Sum64())
}

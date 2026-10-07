package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"sync"
	"time"
)

var t0 = time.Now()

func p(f string, a ...any) { fmt.Printf("%9.3fms "+f+"\n", append([]any{ms(time.Since(t0))}, a...)...) }
func ms(d time.Duration) float64 { return float64(d) / 1e6 }

func serve() {
	go http.ListenAndServe(":8080", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "ok") }))
	l, err := net.Listen("tcp", ":7000")
	if err != nil {
		panic(err)
	}
	for {
		c, err := l.Accept()
		if err != nil {
			continue
		}
		go io.Copy(c, c)
	}
}

func timers() {
	for _, d := range []time.Duration{time.Millisecond, 10 * time.Millisecond, 100 * time.Millisecond, time.Second} {
		for i := 0; i < 3; i++ {
			s := time.Now()
			time.Sleep(d)
			p("sleep %v took %.3fms", d, ms(time.Since(s)))
		}
	}
	s := time.Now()
	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	<-ctx.Done()
	cancel()
	p("ctx 500ms took %.3fms", ms(time.Since(s)))
	// Eight goroutines sleeping at once, like the etcd client's workers.
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for i := 0; i < 3; i++ {
				s := time.Now()
				ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
				<-ctx.Done()
				cancel()
				p("g%d ctx 200ms took %.3fms", g, ms(time.Since(s)))
			}
		}(g)
	}
	wg.Wait()
}

func network() {
	var c net.Conn
	var err error
	for i := 0; i < 50; i++ {
		s := time.Now()
		c, err = net.DialTimeout("tcp", "server:7000", time.Second)
		p("dial %d took %.3fms err=%v", i, ms(time.Since(s)), err)
		if err == nil {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	if err != nil {
		fmt.Println("RESULT fail dial")
		os.Exit(1)
	}
	r := bufio.NewReader(c)
	for i := 0; i < 20; i++ {
		s := time.Now()
		c.SetDeadline(time.Now().Add(2 * time.Second))
		fmt.Fprintf(c, "ping %d\n", i)
		line, err := r.ReadString('\n')
		p("echo %d rtt %.3fms err=%v %q", i, ms(time.Since(s)), err, line)
	}
	cl := &http.Client{Timeout: 2 * time.Second}
	for i := 0; i < 20; i++ {
		s := time.Now()
		resp, err := cl.Get("http://server:8080/")
		if err == nil {
			io.ReadAll(resp.Body)
			resp.Body.Close()
		}
		p("http %d took %.3fms err=%v", i, ms(time.Since(s)), err)
	}
	// Concurrent requests on one connection pool, like gRPC workers.
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for i := 0; i < 5; i++ {
				s := time.Now()
				resp, err := cl.Get("http://server:8080/")
				if err == nil {
					io.ReadAll(resp.Body)
					resp.Body.Close()
				}
				p("g%d http %d took %.3fms err=%v", g, i, ms(time.Since(s)), err)
			}
		}(g)
	}
	wg.Wait()
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "serve" {
		serve()
	}
	timers()
	network()
	fmt.Println("RESULT pass")
}

//go:build !cgo

package main

// The static build (CGO_ENABLED=0, /fuzz/bin/gofz) has no C; cgoThreads runs in /fuzz/bin/gofzcgo.
func cgoThreads(seed uint64, scale int) {
	fail("cgoThreads needs the cgo build (/fuzz/bin/gofzcgo)")
}

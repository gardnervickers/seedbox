// No instrumentation hooks. V8 should optimize this integer computation.
function work(n) {
    let x = 123456789;
    for (let i = 0; i < n; ++i) {
        x ^= x << 13;
        x ^= x >>> 17;
        x ^= x << 5;
        x = (x + i) | 0;
    }
    return x >>> 0;
}
for (let i = 0; i < 20; ++i) work(10000);
console.log(work(Number(process.argv[2] || 100000000)));

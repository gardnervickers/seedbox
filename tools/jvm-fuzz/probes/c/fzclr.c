// Native helper for the .NET probe Cs.reversePInvoke (/fuzz/bin/libfzclr.so): pthreads the runtime
// did not create call managed [UnmanagedCallersOnly] callbacks (reverse P/Invoke) with ten integer
// and nine floating-point arguments, so some of each go on the stack; the callbacks P/Invoke back
// into fz_clr_leaf, which every fourth time calls a second managed callback. Every result is
// checked against the model here. Built with -ffp-contract=off; the floating-point model only
// adds, subtracts and scales by powers of two, so it is exact on either side.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <string.h>

typedef int64_t (*clr_cb)(int64_t a, int32_t b, double c, float d, int64_t e, double f, int64_t g, int64_t h,
                          int64_t i, int64_t j, int64_t k, int64_t l, double m, double n, double o, double p,
                          double q, double r, double s);
typedef int64_t (*clr_leaf_cb)(int64_t x, double y);

static clr_leaf_cb leaf_cb;

static uint64_t xs(uint64_t x) {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    return x;
}

static uint64_t dbits(double d) {
    uint64_t u;
    memcpy(&u, &d, 8);
    return u;
}

static uint32_t fbits(float f) {
    uint32_t u;
    memcpy(&u, &f, 4);
    return u;
}

static double fin(uint64_t x) { return (double)(int64_t)(x >> 11) * 0x1p-33 + ((x & 1) ? -0.75 : 0.25); }

// The model of the second managed callback.
static int64_t leaf2_model(int64_t x, double y) { return (int64_t)(((uint64_t)x * 0x9E3779B97F4A7C15ULL) ^ dbits(y * 2.0)); }

// Called from managed code inside a callback (nested P/Invoke).
int64_t fz_clr_leaf(int64_t x, double y) {
    uint64_t h = (uint64_t)x ^ 0xC2B2AE3D27D4EB4FULL;
    for (int i = 0; i < 16; i++) h = (h ^ (uint64_t)i) * 0x100000001B3ULL;
    h ^= dbits(y - 0.5);
    if ((x & 3) == 1 && leaf_cb) h ^= (uint64_t)leaf_cb((int64_t)h, y);
    return (int64_t)h;
}

static int64_t leaf_model(int64_t x, double y) {
    uint64_t h = (uint64_t)x ^ 0xC2B2AE3D27D4EB4FULL;
    for (int i = 0; i < 16; i++) h = (h ^ (uint64_t)i) * 0x100000001B3ULL;
    h ^= dbits(y - 0.5);
    if ((x & 3) == 1) h ^= (uint64_t)leaf2_model((int64_t)h, y);
    return (int64_t)h;
}

// The model of the managed callback.
static uint64_t cb_model(const uint64_t *w) {
    int64_t ints[10] = {(int64_t)w[0], (int32_t)w[1], (int64_t)w[4], (int64_t)w[6], (int64_t)w[7],
                        (int64_t)w[8], (int64_t)w[9], (int64_t)w[10], (int64_t)w[11], 5};
    uint64_t x = 0x84222325CBF29CE4ULL;
    for (int z = 0; z < 10; z++) x = (x ^ (uint64_t)ints[z]) * 0x9E3779B97F4A7C15ULL + (uint64_t)z;
    double c = fin(w[2]), f = fin(w[5]), m = fin(w[12]), n = fin(w[13]), o = fin(w[14]), p = fin(w[15]),
           q = fin(w[16]), r = fin(w[17]), s = fin(w[18]);
    double sum = c - f * 0.5 + m * 2.0 - n + o * 0.25 + p - q * 4.0 + r + s * 0.125;
    float d = (float)(int32_t)(w[3] >> 40) * 0.5f;
    x ^= dbits(sum) ^ ((uint64_t)fbits(d * 2.0f) << 5);
    return x ^ (uint64_t)leaf_model((int64_t)x, sum);
}

struct clr_arg {
    clr_cb cb;
    int index, calls;
    uint64_t seed, result;
    long mismatch;
};

static void *clr_thread(void *pv) {
    struct clr_arg *a = pv;
    uint64_t x = a->seed * 0x9E3779B97F4A7C15ULL + (uint64_t)a->index + 1, acc = 0;
    for (int k = 0; k < a->calls; k++) {
        uint64_t w[19];
        for (int z = 0; z < 19; z++) w[z] = x = xs(x);
        int64_t got = a->cb((int64_t)w[0], (int32_t)w[1], fin(w[2]), (float)(int32_t)(w[3] >> 40) * 0.5f,
                            (int64_t)w[4], fin(w[5]), (int64_t)w[6], (int64_t)w[7], (int64_t)w[8], (int64_t)w[9],
                            (int64_t)w[10], (int64_t)w[11], fin(w[12]), fin(w[13]), fin(w[14]), fin(w[15]),
                            fin(w[16]), fin(w[17]), fin(w[18]));
        if ((uint64_t)got != cb_model(w)) a->mismatch++;
        acc = acc * 31 + (uint64_t)got;
    }
    a->result = acc;
    return NULL;
}

void fz_clr_set_leaf(clr_leaf_cb cb) { leaf_cb = cb; }

// Runs `threads` new pthreads (at most 16) of `calls` callbacks each; returns the combined result
// and stores the number of wrong results in *mismatch. inl != 0 runs the loops on this thread.
int64_t fz_clr_threads(clr_cb cb, int32_t threads, int32_t calls, int64_t seed, int32_t inl, int64_t *mismatch) {
    pthread_t t[16];
    struct clr_arg a[16];
    if (threads > 16) threads = 16;
    for (int i = 0; i < threads; i++) {
        a[i] = (struct clr_arg){cb, i, calls, (uint64_t)seed, 0, 0};
        if (inl) clr_thread(&a[i]);
        else if (pthread_create(&t[i], NULL, clr_thread, &a[i]) != 0) return -1;
    }
    uint64_t acc = 0;
    *mismatch = 0;
    for (int i = 0; i < threads; i++) {
        if (!inl) pthread_join(t[i], NULL);
        acc = acc * 0x100000001B3ULL ^ a[i].result;
        *mismatch += a[i].mismatch;
    }
    return (int64_t)acc;
}

// JNI and foreign-function helpers for the JVM probes ForeignThreadAttach, JniCriticalGc and
// LibjsigChaining (/fuzz/bin/libfzjni.so). Every value is computed from the probe's seed, so a
// C model and the Java callee must agree bit for bit; built with -ffp-contract=off so neither
// side fuses a multiply and an add.
#define _GNU_SOURCE
#include <jni.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>

static JavaVM *fz_vm;

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved) {
    (void)reserved;
    fz_vm = vm;
    return JNI_VERSION_21;
}

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

// A finite double in [-2^20, 2^20) from a seeded word.
static double fin(uint64_t x) { return (double)(int64_t)(x >> 11) * 0x1p-33 + ((x & 1) ? -0.75 : 0.25); }

// ------------------------------------------------------------------------------------------------
// ForeignThreadAttach: pthreads attach, call Java with many integer and floating-point arguments,
// check every result against this model and detach.

static jclass fta_class;
static jmethodID fta_cb, fta_name, fta_throw, fta_str;
static volatile long fta_mismatch, fta_exceptions;

// The model of ForeignThreadAttach.cb (9 integer and 9 floating-point arguments, so some of each
// are passed on the stack).
static uint64_t cb_model(const jvalue *v) {
    uint64_t h = 0x9E3779B97F4A7C15ULL;
    uint64_t ints[9] = {(uint64_t)v[0].j, (uint64_t)(int64_t)v[1].i, (uint64_t)v[4].j, (uint64_t)(int64_t)v[6].i,
                        (uint64_t)v[13].j, (uint64_t)v[14].j, (uint64_t)v[15].j, (uint64_t)v[16].j, (uint64_t)v[17].j};
    for (int i = 0; i < 9; i++) h = (h ^ ints[i]) * 0x100000001B3ULL + (uint64_t)i;
    double d = v[2].d * 1.5 + v[5].d - v[7].d * 0.25 + v[8].d + v[9].d * 3.0 - v[10].d + v[11].d * 0.125 + v[12].d;
    float f = v[3].f * 2.0f + 1.0f;
    return h ^ dbits(d) ^ ((uint64_t)fbits(f) << 7);
}

static void fill_args(jvalue *v, uint64_t *x) {
    for (int i = 0; i < 18; i++) {
        *x = xs(*x);
        switch (i) {
        case 1: case 6: v[i].i = (jint)*x; break;
        case 3: v[i].f = (float)(int32_t)(*x >> 40) * 0.5f; break;
        case 2: case 5: case 7: case 8: case 9: case 10: case 11: case 12: v[i].d = fin(*x); break;
        default: v[i].j = (jlong)*x; break;
        }
    }
}

struct fta_arg {
    int batch, index, calls, mode;
    uint64_t seed;
    uint64_t result;
};

static uint64_t java_string_hash(const char *s) {
    int32_t h = 0;
    for (; *s; s++) h = (int32_t)((uint32_t)h * 31u + (uint8_t)*s);
    return (uint64_t)(int64_t)h;
}

static int fta_attach(JNIEnv **env, const char *name, int daemon) {
    JavaVMAttachArgs aa = {JNI_VERSION_21, (char *)name, NULL};
    return daemon ? (*fz_vm)->AttachCurrentThreadAsDaemon(fz_vm, (void **)env, &aa)
                  : (*fz_vm)->AttachCurrentThread(fz_vm, (void **)env, &aa);
}

// An unexpected pending exception counts as a mismatch (checking also keeps -Xcheck:jni quiet).
static void fta_unexpected(JNIEnv *env) {
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionClear(env);
        __sync_fetch_and_add(&fta_mismatch, 1);
    }
}

static void *fta_thread(void *p) {
    struct fta_arg *a = p;
    char name[64];
    snprintf(name, sizeof name, "fzf-%d-%d", a->batch, a->index);
    uint64_t x = a->seed * 0x2545F4914F6CDD1DULL + (uint64_t)a->batch * 1000 + (uint64_t)a->index + 1;
    uint64_t acc = 0;
    // mode bit 0: attach as a daemon; bit 1: attach, detach and attach again in the same pthread.
    int attaches = (a->mode & 2) ? 2 : 1;
    for (int at = 0; at < attaches; at++) {
        JNIEnv *env;
        if (fta_attach(&env, name, a->mode & 1) != JNI_OK) {
            __sync_fetch_and_add(&fta_mismatch, 1000);
            return NULL;
        }
        for (int k = 0; k < a->calls; k++) {
            jvalue v[18];
            fill_args(v, &x);
            jlong r = (*env)->CallStaticLongMethodA(env, fta_class, fta_cb, v);
            fta_unexpected(env);
            if ((uint64_t)r != cb_model(v)) __sync_fetch_and_add(&fta_mismatch, 1);
            acc = acc * 31 + (uint64_t)r;
            if (k % 8 == 3) {
                // The callee allocates and returns the hash of its thread's name and daemon flag.
                jlong n = (*env)->CallStaticLongMethod(env, fta_class, fta_name, (jint)k);
                fta_unexpected(env);
                if ((uint64_t)n != (java_string_hash(name) ^ ((uint64_t)(a->mode & 1) << 40))) __sync_fetch_and_add(&fta_mismatch, 1);
            }
            if (k % 8 == 5) {
                // Odd arguments throw; the exception stays pending until cleared here.
                (*env)->CallStaticLongMethod(env, fta_class, fta_throw, (jlong)x);
                int thrown = (*env)->ExceptionCheck(env) == JNI_TRUE;
                if (thrown) (*env)->ExceptionClear(env);
                if (thrown != (int)(x & 1)) __sync_fetch_and_add(&fta_mismatch, 1);
                if (thrown) __sync_fetch_and_add(&fta_exceptions, 1);
            }
            if (k % 8 == 7) {
                char buf[32];
                snprintf(buf, sizeof buf, "s%llx", (unsigned long long)(x & 0xffffff));
                jstring s = (*env)->NewStringUTF(env, buf);
                fta_unexpected(env);
                jlong h = (*env)->CallStaticLongMethod(env, fta_class, fta_str, s);
                fta_unexpected(env);
                (*env)->DeleteLocalRef(env, s);
                if ((uint64_t)h != java_string_hash(buf)) __sync_fetch_and_add(&fta_mismatch, 1);
            }
        }
        if ((*fz_vm)->DetachCurrentThread(fz_vm) != JNI_OK) __sync_fetch_and_add(&fta_mismatch, 1000);
    }
    a->result = acc;
    return NULL;
}

JNIEXPORT void JNICALL Java_ForeignThreadAttach_init(JNIEnv *env, jclass c) {
    fta_class = (*env)->NewGlobalRef(env, c);
    fta_cb = (*env)->GetStaticMethodID(env, c, "cb", "(JIDFJDIDDDDDDJJJJJ)J");
    fta_name = (*env)->GetStaticMethodID(env, c, "nameHash", "(I)J");
    fta_throw = (*env)->GetStaticMethodID(env, c, "maybeThrow", "(J)J");
    fta_str = (*env)->GetStaticMethodID(env, c, "strHash", "(Ljava/lang/String;)J");
}

// Runs one batch of attached pthreads and returns the combined result of their calls.
JNIEXPORT jlong JNICALL Java_ForeignThreadAttach_runBatch(JNIEnv *env, jclass c, jint batch, jint threads, jint calls,
                                                          jlong seed, jint mode) {
    (void)env; (void)c;
    pthread_t t[16];
    struct fta_arg a[16];
    if (threads > 16) threads = 16;
    for (int i = 0; i < threads; i++) {
        a[i] = (struct fta_arg){batch, i, calls, (mode >> (2 * (i % 2))) & 3, (uint64_t)seed, 0};
        if (pthread_create(&t[i], NULL, fta_thread, &a[i]) != 0) return -1;
    }
    uint64_t acc = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(t[i], NULL);
        acc = acc * 0x100000001B3ULL ^ a[i].result;
    }
    return (jlong)acc;
}

JNIEXPORT jlong JNICALL Java_ForeignThreadAttach_mismatches(JNIEnv *env, jclass c) { (void)env; (void)c; return fta_mismatch; }

JNIEXPORT jlong JNICALL Java_ForeignThreadAttach_exceptions(JNIEnv *env, jclass c) { (void)env; (void)c; return fta_exceptions; }

// The foreign-function variant: pthreads HotSpot never saw call an upcall stub, which attaches
// them on first use and detaches them when the pthread exits.
typedef int64_t (*fz_upcall_fn)(int64_t, int32_t, double, float, int64_t, double, int64_t, int64_t, int64_t, int64_t,
                                int64_t, int64_t, double, double, double, double, double, double);

uint64_t fz_upcall_model(int64_t a, int32_t b, double c, float d, int64_t e, double f, int64_t g, int64_t h, int64_t i,
                         int64_t j, int64_t k, int64_t l, double m, double n, double o, double p, double q, double r) {
    uint64_t ints[10] = {(uint64_t)a, (uint64_t)(int64_t)b, (uint64_t)e, (uint64_t)g, (uint64_t)h,
                         (uint64_t)i, (uint64_t)j, (uint64_t)k, (uint64_t)l, 7};
    uint64_t x = 0xC2B2AE3D27D4EB4FULL;
    for (int z = 0; z < 10; z++) x = (x ^ ints[z]) * 0x9E3779B97F4A7C15ULL + (uint64_t)z;
    double s = c - f * 0.5 + m * 2.0 - n + o * 0.25 + p - q * 4.0 + r;
    return x ^ dbits(s) ^ ((uint64_t)fbits(d * 3.0f) << 3);
}

struct ffm_arg {
    fz_upcall_fn fn;
    int index, calls;
    uint64_t seed, result;
    long mismatch;
};

static void *ffm_thread(void *p) {
    struct ffm_arg *a = p;
    uint64_t x = a->seed * 0x9E3779B97F4A7C15ULL + (uint64_t)a->index + 17;
    uint64_t acc = 0;
    for (int k = 0; k < a->calls; k++) {
        uint64_t w[18];
        for (int z = 0; z < 18; z++) w[z] = x = xs(x);
        int64_t r = a->fn((int64_t)w[0], (int32_t)w[1], fin(w[2]), (float)(int32_t)(w[3] >> 40) * 0.5f, (int64_t)w[4],
                          fin(w[5]), (int64_t)w[6], (int64_t)w[7], (int64_t)w[8], (int64_t)w[9], (int64_t)w[10],
                          (int64_t)w[11], fin(w[12]), fin(w[13]), fin(w[14]), fin(w[15]), fin(w[16]), fin(w[17]));
        uint64_t want = fz_upcall_model((int64_t)w[0], (int32_t)w[1], fin(w[2]), (float)(int32_t)(w[3] >> 40) * 0.5f,
                                        (int64_t)w[4], fin(w[5]), (int64_t)w[6], (int64_t)w[7], (int64_t)w[8],
                                        (int64_t)w[9], (int64_t)w[10], (int64_t)w[11], fin(w[12]), fin(w[13]),
                                        fin(w[14]), fin(w[15]), fin(w[16]), fin(w[17]));
        if ((uint64_t)r != want) a->mismatch++;
        acc = acc * 31 + (uint64_t)r;
    }
    a->result = acc;
    return NULL;
}

// Returns the combined result; *mismatch receives the number of wrong upcall results.
int64_t fz_ffm_threads(fz_upcall_fn fn, int32_t threads, int32_t calls, int64_t seed, int64_t *mismatch) {
    pthread_t t[16];
    struct ffm_arg a[16];
    if (threads > 16) threads = 16;
    for (int i = 0; i < threads; i++) {
        a[i] = (struct ffm_arg){fn, i, calls, (uint64_t)seed, 0, 0};
        if (pthread_create(&t[i], NULL, ffm_thread, &a[i]) != 0) return -1;
    }
    uint64_t acc = 0;
    *mismatch = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(t[i], NULL);
        acc = acc * 0x100000001B3ULL ^ a[i].result;
        *mismatch += a[i].mismatch;
    }
    return (int64_t)acc;
}

// ------------------------------------------------------------------------------------------------
// JniCriticalGc: arrays held with GetPrimitiveArrayCritical (nested) during bounded work.

static uint64_t crit_work(jint *a, jint na, jlong *b, jint nb, jdouble *c, jint nc, jint passes, jlong salt) {
    for (int p = 0; p < passes; p++) {
        for (int i = 0; i < na; i++) {
            a[i] = (jint)((uint32_t)a[i] * 0x01000193u ^ (uint32_t)(i + (int32_t)salt + p));
            if (b) b[i % nb] = (jlong)((uint64_t)b[i % nb] + (uint64_t)(int64_t)a[i]);
            if (c) c[i % nc] = c[i % nc] * 0.5 + (double)a[i];
        }
    }
    uint64_t h = 0xcbf29ce484222325ULL;
    for (int i = 0; i < na; i++) h = (h ^ (uint32_t)a[i]) * 0x100000001B3ULL;
    for (int i = 0; b && i < nb; i++) h = (h ^ (uint64_t)b[i]) * 0x100000001B3ULL;
    for (int i = 0; c && i < nc; i++) h = (h ^ dbits(c[i])) * 0x100000001B3ULL;
    return h;
}

// nested: 0 = only a, 1 = a then b, 2 = a, b and c held at once.
JNIEXPORT jlong JNICALL Java_JniCriticalGc_critical(JNIEnv *env, jclass cl, jintArray ja, jlongArray jb, jdoubleArray jc,
                                                    jint passes, jlong salt, jint nested) {
    (void)cl;
    jint na = (*env)->GetArrayLength(env, ja), nb = (*env)->GetArrayLength(env, jb), nc = (*env)->GetArrayLength(env, jc);
    jint *a = (*env)->GetPrimitiveArrayCritical(env, ja, NULL);
    if (!a) return 0;
    jlong *b = nested >= 1 ? (*env)->GetPrimitiveArrayCritical(env, jb, NULL) : NULL;
    jdouble *c = nested >= 2 ? (*env)->GetPrimitiveArrayCritical(env, jc, NULL) : NULL;
    uint64_t h = crit_work(a, na, b, nb, c, nc, passes, salt);
    if (c) (*env)->ReleasePrimitiveArrayCritical(env, jc, c, 0);
    if (b) (*env)->ReleasePrimitiveArrayCritical(env, jb, b, 0);
    (*env)->ReleasePrimitiveArrayCritical(env, ja, a, 0);
    return (jlong)h;
}

// Reads under a critical section and releases with JNI_ABORT (nothing written, so pinning and
// copying agree).
JNIEXPORT jlong JNICALL Java_JniCriticalGc_readAbort(JNIEnv *env, jclass cl, jintArray ja) {
    (void)cl;
    jint n = (*env)->GetArrayLength(env, ja);
    jint *a = (*env)->GetPrimitiveArrayCritical(env, ja, NULL);
    if (!a) return 0;
    uint64_t h = 0;
    for (int i = 0; i < n; i++) h = h * 31 + (uint32_t)a[i];
    (*env)->ReleasePrimitiveArrayCritical(env, ja, a, JNI_ABORT);
    return (jlong)h;
}

JNIEXPORT jlong JNICALL Java_JniCriticalGc_stringCritical(JNIEnv *env, jclass cl, jstring s) {
    (void)cl;
    jsize n = (*env)->GetStringLength(env, s);
    const jchar *p = (*env)->GetStringCritical(env, s, NULL);
    if (!p) return 0;
    int32_t h = 0;
    for (int i = 0; i < n; i++) h = (int32_t)((uint32_t)h * 31u + p[i]);
    (*env)->ReleaseStringCritical(env, s, p);
    return (jlong)h;
}

// The copying interface for contrast: Get/Release<Int>ArrayElements with commit.
JNIEXPORT jlong JNICALL Java_JniCriticalGc_elements(JNIEnv *env, jclass cl, jintArray ja, jint passes, jlong salt) {
    (void)cl;
    jint n = (*env)->GetArrayLength(env, ja);
    jint *a = (*env)->GetIntArrayElements(env, ja, NULL);
    if (!a) return 0;
    uint64_t h = crit_work(a, n, NULL, 1, NULL, 1, passes, salt);
    (*env)->ReleaseIntArrayElements(env, ja, a, 0);
    return (jlong)h;
}

// ------------------------------------------------------------------------------------------------
// LibjsigChaining: a SIGSEGV handler installed from JNI behind HotSpot's (libjsig keeps
// HotSpot's handler and chains to this one for faults HotSpot does not recognize).

#define FZ_PAGES 6
static char *sig_base;
static long sig_page;
static volatile long sig_count[3];  // repaired in a Java thread, repaired in a foreign thread, skipped

// fz_skip_load(p): one load whose fault the handler skips, returning a marker in x0.
__asm__(".text\n.p2align 2\n.globl fz_skip_load\n.hidden fz_skip_load\n.type fz_skip_load,%function\n"
        "fz_skip_load:\n.globl fz_skip_insn\n.hidden fz_skip_insn\nfz_skip_insn:\n\tldr x0, [x0]\n\tret\n"
        ".size fz_skip_load,.-fz_skip_load\n");
extern int64_t fz_skip_load(volatile int64_t *p);
extern char fz_skip_insn[];

#define SKIP_MARK 0x5EED5EED00000000LL

static void sig_handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    char *addr = si->si_addr;
    if (sig_base && addr >= sig_base && addr < sig_base + FZ_PAGES * sig_page) {
        long page = (addr - sig_base) / sig_page;
        if (page >= 4) {
            // Pages 4-5: skip the load and hand back a marker built from the fault address.
            if ((char *)uc->uc_mcontext.pc == fz_skip_insn) {
                uc->uc_mcontext.regs[0] = (uint64_t)SKIP_MARK | (uint64_t)page << 16 | (uint64_t)(addr - sig_base - page * sig_page);
                uc->uc_mcontext.pc += 4;
                __sync_fetch_and_add(&sig_count[2], 1);
                return;
            }
        } else {
            mprotect(sig_base + page * sig_page, sig_page, PROT_READ | PROT_WRITE);
            __sync_fetch_and_add(&sig_count[page >= 2 ? 1 : 0], 1);
            return;
        }
    }
    // Not ours: die with the default action.
    signal(sig, SIG_DFL);
}

// Installs the handler; returns 0, or -1 when the pages cannot be mapped.
JNIEXPORT jint JNICALL Java_LibjsigChaining_install(JNIEnv *env, jclass cl) {
    (void)env; (void)cl;
    sig_page = sysconf(_SC_PAGESIZE);
    void *p = mmap(NULL, FZ_PAGES * sig_page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return -1;
    sig_base = p;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = sig_handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    return sigaction(SIGSEGV, &sa, NULL);
}

static uint64_t fault_loop(int first_page, int n, uint64_t x) {
    uint64_t acc = 0;
    for (int i = 0; i < n; i++) {
        x = xs(x);
        char *page = sig_base + (first_page + (i & 1)) * sig_page;
        mprotect(page, sig_page, PROT_NONE);
        volatile int64_t *slot = (volatile int64_t *)page + (x % 64);
        *slot = (int64_t)x;  // faults; the handler makes the page writable and the store retries
        acc = acc * 31 + (uint64_t)*slot;
    }
    return acc;
}

// Faults n times in the calling (Java) thread on pages 0-1.
JNIEXPORT jlong JNICALL Java_LibjsigChaining_faultLoop(JNIEnv *env, jclass cl, jint n, jlong seed) {
    (void)env; (void)cl;
    return (jlong)fault_loop(0, n, (uint64_t)seed | 1);
}

// n loads from pages 4-5, each skipped by the handler; returns the combined markers.
JNIEXPORT jlong JNICALL Java_LibjsigChaining_skipLoop(JNIEnv *env, jclass cl, jint n, jlong seed) {
    (void)env; (void)cl;
    uint64_t x = (uint64_t)seed | 1, acc = 0;
    for (int i = 0; i < n; i++) {
        x = xs(x);
        volatile int64_t *p = (volatile int64_t *)(sig_base + (4 + (i & 1)) * sig_page) + (x % 512);
        acc = acc * 31 + (uint64_t)fz_skip_load(p);
    }
    return (jlong)acc;
}

struct fault_arg {
    int n;
    uint64_t seed, result;
};

static void *fault_thread(void *p) {
    struct fault_arg *a = p;
    a->result = fault_loop(2, a->n, a->seed);
    return NULL;
}

// Faults from pthreads the JVM does not know (pages 2-3, one thread at a time).
JNIEXPORT jlong JNICALL Java_LibjsigChaining_foreignFaults(JNIEnv *env, jclass cl, jint threads, jint n, jlong seed) {
    (void)env; (void)cl;
    uint64_t acc = 0;
    for (int i = 0; i < threads; i++) {
        pthread_t t;
        struct fault_arg a = {n, (uint64_t)seed + (uint64_t)i * 0x9E37 + 1, 0};
        if (pthread_create(&t, NULL, fault_thread, &a) != 0) return -1;
        pthread_join(t, NULL);
        acc = acc * 0x100000001B3ULL ^ a.result;
    }
    return (jlong)acc;
}

JNIEXPORT jlong JNICALL Java_LibjsigChaining_count(JNIEnv *env, jclass cl, jint path) {
    (void)env; (void)cl;
    return sig_count[path];
}

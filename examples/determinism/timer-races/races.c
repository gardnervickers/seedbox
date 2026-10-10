/* timer-races: timeout-versus-event races armed for the same virtual instant.
 * Each round arms a timeout and an event so both should fire at deadline D and
 * records which side the program observed first. Output: one line per round,
 * per-shape tallies and a sha256 digest of the round lines. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 400
#define MS 1000000LL

/* ---- minimal sha256 ---- */
static const uint32_t K[64] = {
0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
static uint32_t H[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,
                        0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
static unsigned char blk[64];
static size_t blen;
static uint64_t total;
#define ROR(x, n) (((x) >> (n)) | ((x) << (32 - (n))))
static void sha_block(void) {
  uint32_t w[64], a[8];
  for (int i = 0; i < 16; i++)
    w[i] = (uint32_t)blk[4*i] << 24 | (uint32_t)blk[4*i+1] << 16 | (uint32_t)blk[4*i+2] << 8 | blk[4*i+3];
  for (int i = 16; i < 64; i++) {
    uint32_t s0 = ROR(w[i-15], 7) ^ ROR(w[i-15], 18) ^ (w[i-15] >> 3);
    uint32_t s1 = ROR(w[i-2], 17) ^ ROR(w[i-2], 19) ^ (w[i-2] >> 10);
    w[i] = w[i-16] + s0 + w[i-7] + s1;
  }
  memcpy(a, H, sizeof a);
  for (int i = 0; i < 64; i++) {
    uint32_t t1 = a[7] + (ROR(a[4], 6) ^ ROR(a[4], 11) ^ ROR(a[4], 25)) +
                  ((a[4] & a[5]) ^ (~a[4] & a[6])) + K[i] + w[i];
    uint32_t t2 = (ROR(a[0], 2) ^ ROR(a[0], 13) ^ ROR(a[0], 22)) +
                  ((a[0] & a[1]) ^ (a[0] & a[2]) ^ (a[1] & a[2]));
    memmove(a + 1, a, 7 * sizeof a[0]);
    a[4] += t1; a[0] = t1 + t2;
  }
  for (int i = 0; i < 8; i++) H[i] += a[i];
}
static void sha_update(const void *p, size_t n) {
  const unsigned char *c = p;
  for (size_t i = 0; i < n; i++) {
    blk[blen++] = c[i]; total++;
    if (blen == 64) { sha_block(); blen = 0; }
  }
}
static void sha_final(char hex[65]) {
  uint64_t bits = total * 8;
  unsigned char pad = 0x80, z = 0, len[8];
  sha_update(&pad, 1);
  while (blen != 56) sha_update(&z, 1);
  for (int i = 0; i < 8; i++) len[i] = (unsigned char)(bits >> (56 - 8 * i));
  sha_update(len, 8);
  for (int i = 0; i < 8; i++) sprintf(hex + 8 * i, "%08x", H[i]);
}

/* ---- helpers ---- */
static int64_t now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (int64_t)t.tv_sec * 1000000000LL + t.tv_nsec;
}
static struct timespec ts_of(int64_t ns) {
  struct timespec t = {ns / 1000000000LL, ns % 1000000000LL};
  return t;
}
static void die(const char *what) { printf("error: %s %d\n", what, errno); fflush(stdout); exit(1); }

static int64_t deadline;          /* absolute CLOCK_MONOTONIC ns for this round */
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv;
static int flag, wfd;             /* wfd: fd the sibling writes at the deadline */
static volatile sig_atomic_t alarms;

static void sleep_to_deadline(void) {
  struct timespec d = ts_of(deadline);
  while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &d, NULL) == EINTR) {}
}
static void *signaller(void *arg) {
  (void)arg; sleep_to_deadline();
  pthread_mutex_lock(&mu); flag = 1; pthread_cond_signal(&cv); pthread_mutex_unlock(&mu);
  return NULL;
}
static void *writer(void *arg) {
  (void)arg; sleep_to_deadline();
  uint64_t one = 1;               /* 8 bytes for eventfd; pipe reader drains all */
  if (write(wfd, &one, sizeof one) < 0) die("write");
  return NULL;
}
static void on_alarm(int s) { (void)s; alarms++; }
static pthread_t spawn(void *(*fn)(void *)) {
  pthread_t t;
  if (pthread_create(&t, NULL, fn, NULL)) die("pthread_create");
  return t;
}

/* a: pthread_cond_timedwait(D) vs signaller sleeping to D */
static const char *shape_cond(void) {
  pthread_mutex_lock(&mu);
  flag = 0;
  pthread_t t = spawn(signaller);
  struct timespec d = ts_of(deadline);
  int rc = 0;
  while (!flag && rc != ETIMEDOUT) rc = pthread_cond_timedwait(&cv, &mu, &d);
  const char *w = flag ? "signal" : "timeout";
  pthread_mutex_unlock(&mu);
  pthread_join(t, NULL);
  return w;
}

/* b: epoll_wait(1 ms) on a pipe vs writer sleeping to D */
static const char *shape_epoll(void) {
  int p[2], ep = epoll_create1(0);
  if (ep < 0 || pipe(p)) die("pipe");
  struct epoll_event ev = {.events = EPOLLIN}, out;
  if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev)) die("epoll_ctl");
  wfd = p[1];
  pthread_t t = spawn(writer);
  int n = epoll_wait(ep, &out, 1, 1);
  pthread_join(t, NULL);
  close(p[0]); close(p[1]); close(ep);
  return n == 1 ? "ready1" : n == 0 ? "ready0" : "error";
}

/* c: setitimer(1 ms) SIGALRM vs nanosleep(1 ms + offset); winner carries rem ns */
static const char *shape_itimer(int64_t off) {
  static char buf[48];
  sigset_t blk, old;
  sigemptyset(&blk); sigaddset(&blk, SIGALRM);
  alarms = 0;
  struct itimerval it = {{0, 0}, {0, 1000}};
  struct timespec req = ts_of(MS + off), rem = {0, 0};
  if (setitimer(ITIMER_REAL, &it, NULL)) die("setitimer");
  int rc = nanosleep(&req, &rem);
  int eintr = rc != 0 && errno == EINTR;
  snprintf(buf, sizeof buf, "%s:rem%lld", eintr ? "eintr" : "slept",
           (long long)rem.tv_sec * 1000000000LL + rem.tv_nsec);
  sigprocmask(SIG_BLOCK, &blk, &old);   /* make sure the alarm is spent */
  while (!alarms) sigsuspend(&old);
  sigprocmask(SIG_SETMASK, &old, NULL);
  return buf;
}

/* d: timerfd(abs D) vs eventfd written at D, both in one epoll set */
static const char *shape_timerfd(void) {
  static char buf[32];
  int ep = epoll_create1(0);
  int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
  int efd = eventfd(0, 0);
  if (ep < 0 || tfd < 0 || efd < 0) die("fd");
  struct epoll_event ev = {.events = EPOLLIN}, out[2];
  ev.data.u32 = 't'; if (epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev)) die("epoll_ctl");
  ev.data.u32 = 'e'; if (epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev)) die("epoll_ctl");
  struct itimerspec its = {{0, 0}, ts_of(deadline)};
  if (timerfd_settime(tfd, TFD_TIMER_ABSTIME, &its, NULL)) die("timerfd_settime");
  wfd = efd;
  pthread_t t = spawn(writer);
  int n = epoll_wait(ep, out, 2, -1);
  buf[0] = 0;
  for (int i = 0; i < n; i++)
    strcat(buf, i ? (out[i].data.u32 == 't' ? ",timer" : ",event")
                  : (out[i].data.u32 == 't' ? "timer" : "event"));
  pthread_join(t, NULL);
  uint64_t v;
  if (read(tfd, &v, sizeof v) < 0) die("read timerfd");   /* blocks until fired */
  close(tfd); close(efd); close(ep);
  return buf;
}

int main(int argc, char **argv) {
  (void)argc; (void)argv;
  static const char *names[4] = {"cond", "epoll", "itimer", "timerfd"};
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;                 /* no SA_RESTART: nanosleep sees EINTR */
  sigaction(SIGALRM, &sa, NULL);
  pthread_condattr_t ca;
  pthread_condattr_init(&ca);
  pthread_condattr_setclock(&ca, CLOCK_MONOTONIC);
  pthread_cond_init(&cv, &ca);

  static const char *labels[4][2] = {{"timeout", "signal"}, {"ready0", "ready1"},
                                     {"eintr", "slept"}, {"timer_first", "event_first"}};
  int tally[4][2] = {{0}};                  /* [shape][second label observed?] */
  printf("rounds: %d\n", ROUNDS);
  fflush(stdout);
  for (int r = 0; r < ROUNDS; r++) {
    int s = r % 4;
    int64_t off = (r % 7) * 100;
    int64_t t0 = now_ns();
    deadline = t0 + MS + off;
    const char *w = s == 0 ? shape_cond() : s == 1 ? shape_epoll()
                  : s == 2 ? shape_itimer(off) : shape_timerfd();
    int64_t delta = now_ns() - t0;
    tally[s][!strcmp(w, "signal") || !strcmp(w, "ready1") ||
             !strncmp(w, "slept", 5) || !strncmp(w, "event", 5)]++;
    char line[128];
    int n = snprintf(line, sizeof line, "round: %d %s %s %lld\n", r, names[s], w, (long long)delta);
    sha_update(line, (size_t)n);
    fputs(line, stdout);
    fflush(stdout);
  }
  for (int s = 0; s < 4; s++) {
    printf("tally: %s %s=%d %s=%d\n", names[s], labels[s][0], tally[s][0],
           labels[s][1], tally[s][1]);
    fflush(stdout);
  }
  char hex[65];
  sha_final(hex);
  printf("digest: %s\n", hex);
  fflush(stdout);
  return 0;
}

/* Lost-update demo: two threads increment a shared counter. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static volatile long counter;
static long iterations;
static int atomic;

static void *work(void *arg)
{
	(void)arg;
	for (long i = 0; i < iterations; i++) {
		if (atomic) {
			__atomic_fetch_add(&counter, 1, __ATOMIC_SEQ_CST);
		} else {
			long v = counter;
			for (volatile int k = 0; k < 8; k++)
				;
			counter = v + 1;
		}
	}
	return NULL;
}

int main(int argc, char **argv)
{
	iterations = argc > 1 ? atol(argv[1]) : 2000000;
	atomic = argc > 2 && !strcmp(argv[2], "atomic");
	pthread_t a, b;
	pthread_create(&a, NULL, work, NULL);
	pthread_create(&b, NULL, work, NULL);
	pthread_join(a, NULL);
	pthread_join(b, NULL);
	long want = 2 * iterations;
	printf("RACE mode=%s counter=%ld expected=%ld lost=%ld\n", atomic ? "atomic" : "racy", counter, want, want - counter);
	return 0;
}

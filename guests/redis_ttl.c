/* A native application fixture using Redis's own hiredis client library. */
#define _POSIX_C_SOURCE 200809L
#include "hiredis.h"
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static redisContext *commands;
static int negative;
static void fail(const char *message) {
    fprintf(stderr, "FAIL: %s\n", message);
    if (commands && !commands->err) {
        redisReply *reply = redisCommand(commands, "SHUTDOWN NOSAVE");
        if (reply) freeReplyObject(reply);
    }
    exit(1);
}
static void check(int condition, const char *message) { if (!condition) fail(message); }
static long long millis(clockid_t clock) {
    struct timespec ts;
    check(clock_gettime(clock, &ts) == 0, "clock_gettime");
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}
static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    check(nanosleep(&ts, NULL) == 0, "nanosleep");
}
static redisContext *connect_server(void) {
    for (int i = 0; i < 100; i++) {
        redisContext *context = redisConnect("10.0.0.2", 6379);
        if (context && !context->err) return context;
        if (context) redisFree(context);
        sleep_ms(10);
    }
    fail("connect to Redis");
    return NULL;
}
static redisReply *command(const char *text) {
    redisReply *reply = redisCommand(commands, text);
    check(reply != NULL && reply->type != REDIS_REPLY_ERROR, text);
    return reply;
}
static void status(const char *text, const char *expected) {
    redisReply *reply = command(text);
    check(reply->type == REDIS_REPLY_STATUS && strcmp(reply->str, expected) == 0, text);
    freeReplyObject(reply);
}
static long long integer(const char *text) {
    redisReply *reply = command(text);
    check(reply->type == REDIS_REPLY_INTEGER, text);
    long long result = reply->integer;
    freeReplyObject(reply);
    return result;
}
static void value(const char *text, const char *expected) {
    redisReply *reply = command(text);
    check(expected ? (reply->type == REDIS_REPLY_STRING && strcmp(reply->str, expected) == 0)
                   : reply->type == REDIS_REPLY_NIL, text);
    freeReplyObject(reply);
}
static long long expired(void) {
    redisReply *reply = command("INFO stats");
    check(reply->type == REDIS_REPLY_STRING, "INFO stats type");
    char *counter = strstr(reply->str, "expired_keys:");
    check(counter != NULL, "expired_keys counter");
    long long result = strtoll(counter + strlen("expired_keys:"), NULL, 10);
    freeReplyObject(reply);
    return result;
}
int main(int argc, char **argv) {
    negative = argc > 1 && strcmp(argv[1], "--negative-control") == 0;
    commands = connect_server();
    status("PING", "PONG");
    status("SET permanent value", "OK");
    check(integer("TTL permanent") == -1, "persistent TTL");
    check(integer("TTL absent") == -2, "missing TTL");

    status("DEBUG SET-ACTIVE-EXPIRE 0", "OK");
    status(negative ? "SET lazy value PX 60000" : "SET lazy value PX 250", "OK");
    value("GET lazy", "value");
    long long ttl = integer("PTTL lazy");
    check(negative || (ttl > 0 && ttl <= 250), "PTTL before expiration");
    printf("PTTL before sleep: %lld\n", ttl);
    sleep_ms(400);
    check(expired() == 0, "background expiration disabled");
    value("GET lazy", NULL);
    check(integer("PTTL lazy") == -2 && expired() == 1, "lazy expiration");
    puts("PASS lazy expiration");

    char set_absolute[128];
    snprintf(set_absolute, sizeof(set_absolute), "SET absolute value PXAT %lld", millis(CLOCK_REALTIME) + 100);
    status(set_absolute, "OK");
    ttl = integer("PTTL absolute");
    check(ttl > 0 && ttl <= 100, "absolute deadline");
    sleep_ms(150);
    value("GET absolute", NULL);
    check(expired() == 2, "absolute expiration counter");
    puts("PASS absolute expiration");

    status("SET persisted value PX 100", "OK");
    check(integer("PERSIST persisted") == 1, "PERSIST result");
    sleep_ms(150);
    value("GET persisted", "value");
    check(integer("TTL persisted") == -1, "PERSIST removes deadline");
    puts("PASS persisted key");

    status("DEBUG SET-ACTIVE-EXPIRE 1", "OK");
    status("CONFIG SET notify-keyspace-events Ex", "OK");
    redisContext *subscriber = connect_server();
    redisReply *reply = redisCommand(subscriber, "SUBSCRIBE __keyevent@0__:expired");
    check(reply != NULL && reply->type == REDIS_REPLY_ARRAY, "subscribe");
    freeReplyObject(reply);
    long long start = millis(CLOCK_MONOTONIC);
    status("SET active value PX 100", "OK");
    struct pollfd poll_fd = {subscriber->fd, POLLIN, 0};
    check(poll(&poll_fd, 1, 1500) == 1 && (poll_fd.revents & POLLIN), "active expiration notification timeout");
    reply = NULL;
    check(redisGetReply(subscriber, (void **)&reply) == REDIS_OK && reply, "expiration notification");
    check(reply->type == REDIS_REPLY_ARRAY && reply->elements == 3 &&
          reply->element[0]->type == REDIS_REPLY_STRING && strcmp(reply->element[0]->str, "message") == 0 &&
          reply->element[1]->type == REDIS_REPLY_STRING && strcmp(reply->element[1]->str, "__keyevent@0__:expired") == 0 &&
          reply->element[2]->type == REDIS_REPLY_STRING && strcmp(reply->element[2]->str, "active") == 0,
          "active expiration notification contents");
    freeReplyObject(reply);
    long long elapsed = millis(CLOCK_MONOTONIC) - start;
    check(elapsed >= 100 && elapsed <= 1500 && expired() == 3, "active expiration timing");
    printf("PASS active expiration after %lld virtual ms without reading the key\n", elapsed);
    redisFree(subscriber);

    start = millis(CLOCK_MONOTONIC);
    value("BLPOP empty-list 0.2", NULL);
    elapsed = millis(CLOCK_MONOTONIC) - start;
    check(elapsed >= 200 && elapsed <= 500, "blocking command timer");
    printf("PASS blocking command timeout after %lld virtual ms\n", elapsed);
    puts("PASS Redis TTL suite");
    reply = redisCommand(commands, "SHUTDOWN NOSAVE");
    check(reply == NULL && commands->err == REDIS_ERR_EOF, "clean server shutdown");
    redisFree(commands);
    commands = NULL;
    return 0;
}

/* #776 P16: C API, not Rust calls. --test never reads a clock.
 * Build: cc -O2 -std=c11 -Wall -Wextra -Werror -Iinclude bench/nightly.c
 *        target/p16/release/libminigraf.a -lpthread -ldl -lm -o target/p16/nightly
 */
#define _POSIX_C_SOURCE 200809L
#include "minigraf.h"
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define REQUIRE(ok) do { if (!(ok)) { fprintf(stderr, "failed: %s:%d\n", __FILE__, __LINE__); exit(1); } } while (0)

static long long integer(const char *json) {
    const char *v = strstr(json, "\"value\":");
    REQUIRE(v != NULL);
    char *end;
    long long n = strtoll(v + 8, &end, 10);
    REQUIRE(end != v + 8);
    return n;
}
static int even(MiniGrafUdfContext *ctx, void *data, const char *json) {
    (void)ctx; (void)data;
    return integer(json) % 2 == 0;
}
static void *total_new(MiniGrafUdfContext *ctx, void *data) {
    (void)ctx; (void)data;
    void *acc = calloc(1, sizeof(long long));
    REQUIRE(acc != NULL);
    return acc;
}
static int total_step(MiniGrafUdfContext *ctx, void *acc, const char *json) {
    (void)ctx;
    *(long long *)acc += integer(json);
    return 0;
}
static int total_finish(MiniGrafUdfContext *ctx, void *acc, int64_t count) {
    (void)count;
    char out[100];
    snprintf(out, sizeof out, "{\"type\":\"integer\",\"value\":%lld}", *(long long *)acc);
    minigraf_udf_result(ctx, out);
    return 0;
}
/* Only the fixed one-integer-column fixture is decoded here. Consume every
 * scalar and check count AND sum; an empty/error cursor cannot look successful. */
typedef struct { long count; long long sum; } Answer;
static Answer decode(const char *json) {
    REQUIRE(json != NULL);
    const char *p = strstr(json, "\"results\":");
    p = p ? strchr(p, '[') : json;
    Answer a = {0, 0};
    int depth = 0;
    for (; *p; p++) {
        if (*p == '[') depth++;
        if (*p == ']' && --depth == 0) break;
        if (isdigit((unsigned char)*p) || *p == '-') {
            char *end;
            a.sum += strtoll(p, &end, 10);
            REQUIRE(end > p);
            a.count++;
            p = end - 1;
        }
    }
    return a;
}
static MiniGrafPrepared *prepare(MiniGrafDb *db, const char *q) {
    MiniGrafPrepared *p = minigraf_prepare(db, q);
    REQUIRE(p != NULL);
    return p;
}
static double now_ns(void) {
    struct timespec t;
    REQUIRE(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    return (double)t.tv_sec * 1e9 + t.tv_nsec;
}
static int compare(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}
int main(int argc, char **argv) {
    REQUIRE(argc == 1 || (argc == 2 && strcmp(argv[1], "--test") == 0));
    int test = argc == 2;
    MiniGrafDb *db = minigraf_open_in_memory();
    REQUIRE(db != NULL);
    char statement[40000];
    size_t at = (size_t)sprintf(statement, "(transact [");
    for (int i = 0; i < 1000; i++) at += (size_t)sprintf(statement + at, "[:e%d :n %d] ", i, i);
    strcpy(statement + at, "])");
    char *r = minigraf_execute(db, statement);
    REQUIRE(r != NULL); minigraf_string_free(r);
    r = minigraf_execute(db, "(transact [[:tags :tags #{1 \"x\"}]])");
    REQUIRE(r != NULL); minigraf_string_free(r);
    REQUIRE(minigraf_register_predicate(db, "bench-even?", even, NULL, NULL) == 0);
    REQUIRE(minigraf_register_aggregate(db, "bench-total", total_new, total_step, total_finish, free, NULL, NULL) == 0);
    MiniGrafPrepared *queries[] = {
        prepare(db, "(query [:find ?n :where [?e :n $n] [?e :n ?n]])"),
        prepare(db, "(query [:find ?n :where [?e :n ?n] [(>= ?n $min)]])"),
        prepare(db, "(query [:find ?s :where [:tags :tags $s] [:tags :tags ?s]])"),
        prepare(db, "(query [:find ?n :where [?e :n ?n] [(bench-even? ?n)]])"),
        prepare(db, "(query [:find (bench-total ?n) :where [?e :n ?n]])")
    };
    const char *ids[] = {"prepared_execute/1k", "cursor_batch_1/1k", "cursor_batch_256/1k", "set_roundtrip/2", "predicate/1k", "aggregate/1k"};
    const long counts[] = {1, 1000, 1000, 1, 500, 1};
    const long long sums[] = {500, 499500, 499500, 1, 249500, 499500};
    FILE *output = test ? NULL : fopen("binding-results.json", "w");
    REQUIRE(test || output != NULL);
    if (output) fputs("{\n", output);
    for (int c = 0; c < 6; c++) {
        double samples[10];
        for (int s = 0; s < (test ? 1 : 10); s++) {
            double start = test ? 0 : now_ns();
            Answer a = {0, 0};
            if (c == 1 || c == 2) {
                MiniGrafCursor *cursor = minigraf_prepared_query(queries[1], "{\"min\":0}");
                REQUIRE(cursor != NULL);
                while ((r = minigraf_cursor_next_batch(cursor, c == 1 ? 1 : 256)) != NULL) {
                    Answer batch = decode(r);
                    a.count += batch.count; a.sum += batch.sum;
                    minigraf_string_free(r);
                }
                REQUIRE(minigraf_cursor_last_error(cursor) == NULL);
                minigraf_cursor_free(cursor);
            } else {
                int q = c == 0 ? 0 : c - 1;
                r = minigraf_prepared_execute(queries[q], c == 0 ? "{\"n\":500}" : c == 3 ? "{\"s\":{\"type\":\"set\",\"value\":[1,\"x\"]}}" : "{}");
                REQUIRE(r != NULL);
                if (c == 3) {
                    REQUIRE(strstr(r, "[[[\"x\",1]]]") != NULL);
                    a = (Answer){1, 1};
                } else a = decode(r);
                minigraf_string_free(r);
            }
            if (!test) samples[s] = now_ns() - start;
            REQUIRE(a.count == counts[c] && a.sum == sums[c]);
        }
        printf("ok: binding/c/%s\n", ids[c]);
        if (output) {
            qsort(samples, 10, sizeof(double), compare);
            fprintf(output, "%s\"binding/c/%s\":{\"latency\":{\"value\":%.3f}}\n", c ? "," : "", ids[c], (samples[4] + samples[5]) / 2);
        }
    }
    if (output) { fputs("}\n", output); REQUIRE(fclose(output) == 0); }
    for (int i = 0; i < 5; i++) minigraf_prepared_free(queries[i]);
    minigraf_close(db);
    return 0;
}

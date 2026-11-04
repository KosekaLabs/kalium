/*
 * An ordinary C program: stdio formatting, malloc/free, qsort, strtod/strtol,
 * snprintf, recursion, time(), reading stdin (fgets, readv, read), and an
 * exit code. Nothing in it knows about Kalium; it is compiled with clang
 * against wasi-libc for wasm32-wasi and talks to the kernel's WASI preview1
 * layer through the libc.
 *
 * Boot image module 9 (`hello_c`); `init` spawns it with a console capability
 * in slot 0 (its stdout/stderr) and the receive end of a channel in slot 1
 * (its stdin, which init fills with "hello\n", "world" and an empty message
 * before the spawn), and expects exit code 5. See README.md next to this file
 * for the compiler invocation.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static int compare_ints(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static unsigned long factorial(unsigned n) {
    return n <= 1 ? 1UL : n * factorial(n - 1);
}

static unsigned fib(unsigned n) {
    return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

int main(int argc, char **argv) {
    printf("hello_c: hello from C on Kalium\n");

    /* printf with several conversions. */
    printf("hello_c: printf %d|%5d|%-5d|%x|%o|%c|%s|%5.2f|%e|%g|%%\n", 42, 42, 42, 255,
           8, 'K', "str", 3.14159, 1234.5, 0.0001);
    printf("hello_c: argc=%d argv0=%s\n", argc, argc > 0 ? argv[0] : "(none)");

    /* malloc/free of a few blocks, with a pattern check. */
    size_t sizes[3] = {16, 1000, 70000};
    int ok = 1;
    for (int i = 0; i < 3; i++) {
        unsigned char *p = malloc(sizes[i]);
        if (p == NULL) {
            ok = 0;
            continue;
        }
        memset(p, (unsigned char)(0x5a + i), sizes[i]);
        for (size_t j = 0; j < sizes[i]; j += 97)
            ok &= p[j] == (unsigned char)(0x5a + i);
        free(p);
    }
    printf("hello_c: malloc/free %s (%d blocks)\n", ok ? "ok" : "FAILED", 3);

    /* qsort. */
    int nums[] = {21, 5, 13, 1, 8, 2, 3};
    qsort(nums, sizeof nums / sizeof nums[0], sizeof nums[0], compare_ints);
    printf("hello_c: qsort [");
    for (size_t i = 0; i < sizeof nums / sizeof nums[0]; i++)
        printf("%s%d", i ? ", " : "", nums[i]);
    printf("]\n");

    /* strtod / strtol. */
    char *end = NULL;
    double d = strtod("3.14159 tail", &end);
    long l1 = strtol("42", NULL, 10), l2 = strtol("-17", NULL, 10), l3 = strtol("ff", NULL, 16);
    printf("hello_c: strtod %.5f rest \"%s\", strtol %ld %ld %ld\n", d, end, l1, l2, l3);

    /* snprintf into a bounded buffer, including truncation. */
    char buf[32];
    int n = snprintf(buf, sizeof buf, "x=%d y=%.2f z=%s", 7, 3.5, "end");
    char small[8];
    int m = snprintf(small, sizeof small, "%s", "truncated");
    printf("hello_c: snprintf \"%s\" (%d), \"%s\" (%d)\n", buf, n, small, m);

    /* Recursion. */
    printf("hello_c: fact(10)=%lu fib(20)=%u\n", factorial(10), fib(20));

    /* time(): WASI clock_time_get(REALTIME); on Kalium that counts from boot. */
    time_t t = time(NULL);
    printf("hello_c: time() %s (%lld s)\n", t >= 0 ? "ok" : "failed", (long long)t);

    /*
     * stdin: on Kalium the receive end of a channel, each message one chunk
     * of input. init queued "hello\n", "world" and an empty message (end of
     * file) before spawning us and keeps the sending end until we have
     * exited, so the stream is never closed from the other side: the
     * zero-length read at the end must return at once on its own.
     */

    /* A line through stdio (the FILE buffer takes the whole message). */
    char line[16];
    if (fgets(line, sizeof line, stdin) == NULL)
        snprintf(line, sizeof line, "%s", feof(stdin) ? "(eof)" : "(error)");
    line[strcspn(line, "\n")] = '\0';
    printf("hello_c: stdin fgets \"%s\"\n", line);

    /* readv with an empty first buffer: POSIX skips it, the data lands in
     * the second, and the count is the data's. */
    char chunk[16] = {0};
    struct iovec iov[2] = {{chunk, 0}, {chunk, sizeof chunk - 1}};
    ssize_t got = readv(0, iov, 2);
    printf("hello_c: stdin readv (empty, 15) got %zd \"%s\"\n", got, got > 0 ? chunk : "");

    /* The empty message: end of file, 0 bytes. Reads until then, so that
     * stdin is drained whatever the readv above left behind. */
    do
        got = read(0, chunk, sizeof chunk);
    while (got > 0);
    printf("hello_c: stdin read got %zd (%s)\n", got, got == 0 ? "end of file" : "error");

    /* A read of 0 bytes with nothing queued: 0 at once, without waiting
     * for input (the sender is still open; waiting here would deadlock
     * against init, which is waiting for us). */
    got = read(0, chunk, 0);
    printf("hello_c: stdin read of 0 bytes got %zd at once\n", got);

    printf("hello_c: exiting with 5\n");
    return 5;
}

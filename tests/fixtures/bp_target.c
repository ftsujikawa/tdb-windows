#include <stdio.h>
#include <stdlib.h>

/* Fixture for breakpoint / step / backtrace / memory / expression-evaluation
   / leak-detection tests (see docs/04_テスト仕様書.md). Line numbers below
   are load-bearing: several tests reference "bp_target.c:<line>" directly,
   so keep existing lines stable and only ever append new ones at the end. */

struct test_struct {
    int a;
    int b;
};

int add(int a, int b)
{
    return a + b;
}

int main(void)
{
    int x = 10;
    struct test_struct s = {1, 2};
    int arr[5] = {1, 2, 3, 4, 5};
    struct test_struct *ps = &s;
    char *str = "Hello, World!";

    printf("hello\n");

    x++;
    printf("%d\n", x);

    printf("%d\n", add(5, 3));

    printf("%d %d\n", s.a, s.b);
    printf("%d %d %d %d %d\n", arr[0], arr[1], arr[2], arr[3], arr[4]);
    printf("%d\n", ps->a);
    printf("%s\n", str);

    /* leak_ptr is intentionally never freed; ok_ptr is freed right away.
       Used by the leak-detection tests to confirm outstanding vs. reclaimed
       allocations are told apart. */
    void *leak_ptr = malloc(64);
    void *ok_ptr = malloc(32);
    free(ok_ptr);

    printf("done\n");
    return 0;
}

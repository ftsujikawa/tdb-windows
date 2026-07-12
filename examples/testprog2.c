#include <stdio.h>
#include <stdlib.h>

/* union: exercises the print-pretty "union" prefix (not covered by testprog.c,
   which only has struct types) */
union test_union
{
    int i;
    float f;
    char bytes[4];
};

/* globals: for the `show globals` command */
int g_counter = 0;
const char *g_message = "global message";

/* recursive function: produces multiple stack frames for `tb`/`up` */
int factorial(int n)
{
    if (n <= 1)
        return 1;
    return n * factorial(n - 1);
}

/* function with a parameter and a local: for `show args`/`show locals`,
   and a loop body to step/next through */
int sum_loop(int count)
{
    int total = 0;

    for (int i = 0; i < count; i++)
    {
        total += i;
        g_counter++;
    }

    return total;
}

int main(void)
{
    union test_union u;
    u.i = 65; /* 'A' */

    printf("union.i = %d\n", u.i);

    float f = 2.3;
    printf("float f = %f\n", f);
    
    int fact5 = factorial(5);
    printf("factorial(5) = %d\n", fact5);

    int total = sum_loop(10);
    printf("sum_loop(10) = %d\n", total);

    void *leak = malloc(48); /* intentionally never freed: leak-detection demo */
    (void)leak;
    //free(leak);

    printf("g_counter = %d\n", g_counter);

    int i = 0;
    while(1) {
        printf("result = %d\n", i++);
    }

    return 0;
}

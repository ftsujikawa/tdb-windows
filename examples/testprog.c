#include <stdio.h>
#include <stdlib.h>

struct test_struct {
    int a;
    int b;
};

struct test2_struct
{
    int x;
    int y;
    struct test_struct z;
};

int add(int a, int b)
{
    return a + b;
}

int main()
{
    int x = 10;
    struct test_struct s = {1, 2};
    int a[5] = {1, 2, 3, 4, 5};
    struct test_struct *ps = &s;
    struct test_struct sa[3] = {{1, 2}, {3, 4}, {5, 6}};
    struct test2_struct t2s = {10, 20, {30, 40}};
    struct test2_struct a2s[2] = {{10, 20, {30, 40}}, {50, 60, {70, 80}}};
    char *str = "Hello, World!";
    
    printf("hello\n");

    x++;

    printf("%d\n", x);

    printf("%d\n", add(5, 3));

    printf("%d\n", s.a);
    printf("%d\n", s.b);

    printf("%d\n", a[0]);
    printf("%d\n", a[1]);
    printf("%d\n", a[2]);
    printf("%d\n", a[3]);
    printf("%d\n", a[4]);

    void *leak_ptr = malloc(64);   /* intentionally never freed: leak-detection demo */
    void *ok_ptr = malloc(32);
    free(ok_ptr);

    return 0;
}
#include <stdio.h>

int add(int a, int b) {
    return a + b;
}

int main() {
    for (int i = 0; i < 5; i++) {
        int result = add(i, i * 2);
        printf("i=%d result=%d\n", i, result);
    }
    return 0;
}

#include <stdio.h>

int unit_a(void);
int unit_b(void);

int main(void) {
    printf("%d\n", unit_a() + unit_b());
    return 0;
}

#include <stdio.h>

#include "config.h"
#include "util.h"

int main(void) {
    printf("%s %d\n", FROST_GREETING, frost_add(40, 2));
    return 0;
}

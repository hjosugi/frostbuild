#include "util.h"
#include "util_internal.h"

int frost_add(int a, int b) {
    return a + b + FROST_INTERNAL_BIAS;
}

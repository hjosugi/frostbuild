#ifndef FROST_SAMPLE_UTIL_INTERNAL_H
#define FROST_SAMPLE_UTIL_INTERNAL_H

/* Only util.c includes this header; main.c must not. The incremental e2e
 * test edits it and asserts that only util.c is recompiled. */
#define FROST_INTERNAL_BIAS 0

#endif

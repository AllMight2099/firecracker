// SPDX-License-Identifier: Apache-2.0
//
// Guest init program for the deterministic replay demo. Runs as PID 1
// inside an initramfs, prints a small deterministic transcript to the
// serial console, then halts.

#include <stdio.h>

static void busy_work(void) {
    volatile unsigned long k = 0;
    for (unsigned long i = 0; i < 20000000UL; i++) k += i;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    puts("det-replay demo: hello from the guest");
    for (int i = 0; i < 5; i++) {
        printf("det-replay demo: tick %d\n", i);
        busy_work();
    }
    puts("det-replay demo: done, halting");
    for (;;) { busy_work(); }
    return 0;
}

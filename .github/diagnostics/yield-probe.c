#include <sched.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <execinfo.h>
static unsigned long count;
static double total_ms, maximum_ms;
static double now_ms(void) {struct timespec ts;clock_gettime(CLOCK_MONOTONIC,&ts);return ts.tv_sec*1000.0+ts.tv_nsec/1e6;}
static int probe_yield(void) {
 if (!pthread_main_np()) return sched_yield();
 double start=now_ms();
 if(count++==0) {void *frames[20];int n=backtrace(frames,20);backtrace_symbols_fd(frames,n,2);}
 if(getenv("HTTP3_INJECT_YIELD_DELAY")) {struct timespec delay={0,10000000};nanosleep(&delay,NULL);}
 int result=sched_yield();
 double elapsed=now_ms()-start;total_ms+=elapsed;if(elapsed>maximum_ms)maximum_ms=elapsed;
 return result;
}
__attribute__((destructor)) static void report(void) {fprintf(stderr,"YIELD_PROBE pid=%d count=%lu total_ms=%.3f max_ms=%.3f\n",getpid(),count,total_ms,maximum_ms);}
__attribute__((used)) static struct {const void *replacement; const void *original;} interpose __attribute__((section("__DATA,__interpose")))={(const void*)probe_yield,(const void*)sched_yield};

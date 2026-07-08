#pragma once
#include <sys/types.h>
#include <unistd.h>
#define getrandom(buf, len, flags) (getentropy((buf), (len)) == 0 ? (ssize_t)(len) : (ssize_t)-1)
#define socket(a, b, c) (-1)
#define setsockopt(a, b, c, d, e) (-1)
#define connect(a, b, c) (-1)

#ifndef SYSROOT_SYS_SHM_H_
#define SYSROOT_SYS_SHM_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <features.h>

#define __NEED_time_t
#define __NEED_size_t
#define __NEED_pid_t

#include <sys/ipc.h>

#include <bits/alltypes.h>

#ifdef _GNU_SOURCE
#define __used_ids used_ids
#define __swap_attempts swap_attempts
#define __swap_successes swap_successes
#endif

#define SHM_R 0400
#define SHM_W 0200

#define SHM_RDONLY 010000
#define SHM_RND 020000
#define SHM_REMAP 040000
#define SHM_EXEC 0100000

#define SHM_LOCK 11
#define SHM_UNLOCK 12
#define SHM_STAT 13
#define SHM_INFO 14
#define SHM_DEST 01000
#define SHM_LOCKED 02000
#define SHM_HUGETLB 04000
#define SHM_NORESERVE 010000

typedef unsigned long shmatt_t;

void* shmat(int, const void*, int);
int shmctl(int, int, void*);
int shmdt(const void*);
int shmget(key_t, size_t, int);

#ifdef __cplusplus
}
#endif

#endif  // SYSROOT_SYS_SHM_H_

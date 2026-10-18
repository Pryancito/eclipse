/*
 * OUR code, not NVIDIA's -- what eclipse_rm_mem.c's system-memory
 * allocations look like to the rest of Eclipse's own C.
 *
 * The memData of every ADDR_SYSMEM memdesc osAllocPagesInternal allocated
 * (pMemDesc->Allocated) is one of these. On Linux that slot holds an
 * nv_alloc_t; eclipse_rm_frontend.c reads this instead when a client maps
 * its own allocation through /dev/nvidiactl.
 */
#ifndef ECLIPSE_RM_MEM_H
#define ECLIPSE_RM_MEM_H

#include "nvtypes.h"

typedef struct
{
    NvU64 physBase;   /* physical base of the contiguous block           */
    NvU64 pageCount;  /* number of RM_PAGE_SIZE (4 KiB) pages allocated  */
    NvU32 cacheType;  /* NV_MEMORY_* the CPU was asked to use for it,
                       * nv_alloc_t's cache_type                         */
} EclipseSysmemAlloc;

/*
 * Whether a front-end call (a user-mode client's escape) is in flight
 * (eclipse_rm_frontend.c). The user-mapping entry points answer only then;
 * outside one they keep answering NV_ERR_NOT_SUPPORTED, as they always did.
 */
NvBool eclipse_rm_frontend_in_call(void);

#endif /* ECLIPSE_RM_MEM_H */

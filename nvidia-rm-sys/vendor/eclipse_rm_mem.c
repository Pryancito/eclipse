/*
 * OUR code, not NVIDIA's -- Eclipse's implementation of the four
 * os-interface.h system-memory entry points the real RM calls for every
 * ADDR_SYSMEM memory descriptor:
 *
 *     osAllocPagesInternal / osFreePagesInternal
 *     osMapSystemMemory    / osUnmapSystemMemory
 *
 * and, at the end, of osCreateMemFromOsDescriptor: the one descriptor
 * /dev/nvidia-uvm needs, of pages it lends the RM.
 *
 * NVIDIA's own versions live in arch/nvalloc/unix/src/os.c -- the Linux
 * platform layer Eclipse does not vendor -- and are built on nv_alloc_pages
 * / nv_alloc_kernel_mapping / nv_state_t. Eclipse instead backs them with
 * its own contiguous DMA frame allocator and kernel physmap, already
 * exported as C symbols by kernel-hal (drivers.rs, #[no_mangle]) and used
 * the same way by the AHCI/NVMe/e1000e drivers. Those symbols are resolved
 * at the final zcore link, exactly like glue.c's nvrm_shim_log_raw.
 *
 * Contract (transcribed from the real os.c + mem_desc.c):
 *   - _memdescAllocInternal (mem_desc.c) calls osAllocPages(pMemDesc) for
 *     ADDR_SYSMEM. It must fill the memdesc's CPU PTE array with physical
 *     page addresses and stash an opaque per-alloc handle via
 *     memdescSetMemData so the free/map paths can find the block again.
 *   - memdescMap -> osMapSystemMemory turns that handle into a CPU kernel
 *     virtual address (GspMsgQueuesInit fails with NV_ERR_NO_MEMORY when
 *     this returns NULL -- message_queue_cpu.c:288).
 *
 * Eclipse allocates the whole descriptor as one physically contiguous DMA
 * block (valid for both contiguous and non-contiguous memdescs -- the PTE
 * array is just filled with base + i*RM_PAGE_SIZE), and every physical page
 * of RAM is already mapped in the kernel physmap, so a kernel mapping is
 * just drivers_phys_to_virt(base) with no per-mapping page-table work.
 */
#include "os/os.h"
#include "nvos.h"
#include "gpu/gpu.h"
#include "gpu/mem_mgr/mem_desc.h"
#include "gpu/mem_mgr/rm_page_size.h"
#include "nvport/nvport.h"
#include "rmapi/client.h"
#include "rmapi/rs_utils.h"
#include "eclipse_rm_mem.h"

/* Eclipse kernel-hal seams (kernel-hal/src/drivers.rs, all #[no_mangle]).
 * usize/PhysAddr/VirtAddr are all 64-bit on x86_64, matching NvU64. */
extern NvU64 drivers_dma_alloc(NvU64 pages);
extern NvS32 drivers_dma_dealloc(NvU64 paddr, NvU64 pages);
extern NvU64 drivers_phys_to_virt(NvU64 paddr);
extern NvS32 drivers_dma_mark_uncached(NvU64 paddr, NvU64 pages);

/*
 * memData handle (EclipseSysmemAlloc, eclipse_rm_mem.h): remembers the
 * contiguous block behind a memdesc so osFreePagesInternal can return it to
 * the frame allocator and osMapSystemMemory can recover its base directly
 * (rather than re-deriving it from a possibly IOMMU-translated PTE array).
 * The one memdesc of system memory whose memData is something else is a
 * client's own pages (osCreateMemFromOsDescriptor, at the end).
 */
static NvBool _eclipseIsUserPages(MEMORY_DESCRIPTOR *pMemDesc);
static NV_STATUS _eclipseMapUserPages(MEMORY_DESCRIPTOR *pRoot, NvU64 offset,
                                      NvU64 length, NvBool Kernel,
                                      NvP64 *ppAddress);

NV_STATUS osAllocPagesInternal(MEMORY_DESCRIPTOR *pMemDesc)
{
    NvU64               size       = memdescGetSize(pMemDesc);
    NvU64               pageCount  = (size + RM_PAGE_SIZE - 1) / RM_PAGE_SIZE;
    NvU64               physBase;
    NvU32               pteCount;
    NvU32               i;
    RmPhysAddr         *pteArray;
    EclipseSysmemAlloc *pAlloc;
    NV_STATUS           status;

    memdescSetMemData(pMemDesc, NULL, NULL);

    if (pageCount == 0)
        return NV_ERR_INVALID_ARGUMENT;

    physBase = drivers_dma_alloc(pageCount);
    if (physBase == 0)
        return NV_ERR_NO_MEMORY;

    /* Honor an uncached request the same way the Linux path's GFP flags do,
     * by remapping the physmap image of these pages UC (drivers.rs). The
     * GSP RPC queues are NV_MEMORY_CACHED and skip this; the sysmem flush
     * buffer is NV_MEMORY_UNCACHED and needs it. */
    if (memdescGetCpuCacheAttrib(pMemDesc) == NV_MEMORY_UNCACHED)
        (void) drivers_dma_mark_uncached(physBase, pageCount);

    /*
     * Fill the CPU PTE array. memdescGetPteArraySize returns 1 for a
     * physically-contiguous memdesc (single base entry) and PageCount for a
     * non-contiguous one (one entry per page); a contiguous block satisfies
     * both because entry i is simply base + i*RM_PAGE_SIZE.
     */
    pteArray = memdescGetPteArray(pMemDesc, AT_CPU);
    pteCount = memdescGetPteArraySize(pMemDesc, AT_CPU);
    for (i = 0; i < pteCount; i++)
        pteArray[i] = physBase + (NvU64) i * RM_PAGE_SIZE;

    /* Record alloc bookkeeping just like the real osAllocPagesInternal
     * (os_page_size == RM_PAGE_SIZE on x86_64, so no OS/RM page inflation). */
    status = memdescSetAllocSizeFields(pMemDesc, pageCount * RM_PAGE_SIZE, RM_PAGE_SIZE);
    if (status != NV_OK)
    {
        (void) drivers_dma_dealloc(physBase, pageCount);
        return status;
    }

    pAlloc = portMemAllocNonPaged(sizeof(*pAlloc));
    if (pAlloc == NULL)
    {
        (void) drivers_dma_dealloc(physBase, pageCount);
        return NV_ERR_NO_MEMORY;
    }
    pAlloc->physBase  = physBase;
    pAlloc->pageCount = pageCount;
    pAlloc->cacheType = memdescGetCpuCacheAttrib(pMemDesc);

    memdescSetMemData(pMemDesc, pAlloc, NULL);
    return NV_OK;
}

void osFreePagesInternal(MEMORY_DESCRIPTOR *pMemDesc)
{
    EclipseSysmemAlloc *pAlloc = (EclipseSysmemAlloc *) memdescGetMemData(pMemDesc);

    if (pAlloc == NULL)
        return;

    (void) drivers_dma_dealloc(pAlloc->physBase, pAlloc->pageCount);
    portMemFree(pAlloc);
    memdescSetMemData(pMemDesc, NULL, NULL);
}

NV_STATUS osMapSystemMemory(
    MEMORY_DESCRIPTOR *pMemDesc,
    NvU64              Offset,
    NvU64              Length,
    NvBool             Kernel,
    NvU32              Protect,
    NvP64             *ppAddress,
    NvP64             *ppPrivate
)
{
    NvU64               rootOffset = 0;
    MEMORY_DESCRIPTOR  *pRoot      = memdescGetRootMemDesc(pMemDesc, &rootOffset);
    EclipseSysmemAlloc *pAlloc;
    NvU64               va;

    (void) Protect;

    *ppAddress = NvP64_NULL;
    *ppPrivate = NvP64_NULL;

    if ((Offset + Length) < Length)
        return NV_ERR_INVALID_ARGUMENT;
    if ((Offset + Length) > memdescGetSize(pMemDesc))
        return NV_ERR_INVALID_ARGUMENT;

    /*
     * A user mapping exists only for a user-mode client's RM_MAP_MEMORY
     * (eclipse_rm_frontend.c), and outside one this answers what it always
     * did: every RM-internal allocation maps with Kernel=NV_TRUE.
     */
    if (!Kernel && !eclipse_rm_frontend_in_call())
        return NV_ERR_NOT_SUPPORTED;

    if (_eclipseIsUserPages(pRoot))
        return _eclipseMapUserPages(pRoot, rootOffset + Offset, Length,
                                    Kernel, ppAddress);

    pAlloc = (EclipseSysmemAlloc *) memdescGetMemData(pRoot);
    if (pAlloc == NULL)
        return NV_ERR_INVALID_STATE;

    /*
     * nv_alloc_user_mapping (nv-usermap.c): the "address" handed to user
     * space is the physical address of the byte asked for. It is not mapped
     * by this; the client mmaps the file RM_MAP_MEMORY attached the mapping
     * to, and RM_MAP_MEMORY finds the allocation again from this address
     * (os_match_mmap_offset below).
     */
    if (!Kernel)
    {
        *ppAddress = (NvP64) (pAlloc->physBase + rootOffset + Offset);
        *ppPrivate = NvP64_NULL;
        return NV_OK;
    }

    /* Eclipse maps all physical RAM in the kernel physmap, so a kernel VA is
     * just the physmap image of the block's physical base -- no per-mapping
     * page-table work, and thus nothing for osUnmapSystemMemory to unwind. */
    va = drivers_phys_to_virt(pAlloc->physBase + rootOffset + Offset);
    if (va == 0)
        return NV_ERR_GENERIC;

    *ppAddress = NV_PTR_TO_NvP64((void *) (NvUPtr) va);
    *ppPrivate = NvP64_NULL;
    return NV_OK;
}

/* 580.178.04 dropped the ProcessId argument (same change as memdescUnmap):
 * it was only ever meaningful for user-space mappings. */
void osUnmapSystemMemory(
    MEMORY_DESCRIPTOR *pMemDesc,
    NvBool             Kernel,
    NvP64              pAddress,
    NvP64              pPrivate
)
{
    /* Physmap image -- the mapping is not a separate allocation, so there is
     * nothing to tear down here (the pages are released in
     * osFreePagesInternal). */
    (void) pMemDesc;
    (void) Kernel;
    (void) pAddress;
    (void) pPrivate;
}

/*
 * os_match_mmap_offset (os-usermap.c): which page of the allocation
 * `pAllocPrivate` the page-aligned user address `offset` names, for
 * RM_MAP_MEMORY to validate a system-memory mapping against the memory
 * handle it was made for. The block is contiguous, so that is the page
 * whose physical address it is.
 */
NV_STATUS os_match_mmap_offset(
    void  *pAllocPrivate,
    NvU64  offset,
    NvU64 *pPageIndex
)
{
    EclipseSysmemAlloc *pAlloc = (EclipseSysmemAlloc *) pAllocPrivate;
    NvU64 index;

    if ((pAlloc == NULL) || (pPageIndex == NULL))
        return NV_ERR_OBJECT_NOT_FOUND;

    /* Linux walks the page table for an exact match; on one contiguous
     * block that is this arithmetic. */
    if ((offset < pAlloc->physBase) ||
        (((offset - pAlloc->physBase) % RM_PAGE_SIZE) != 0))
        return NV_ERR_OBJECT_NOT_FOUND;

    index = (offset - pAlloc->physBase) / RM_PAGE_SIZE;
    if (index >= pAlloc->pageCount)
        return NV_ERR_OBJECT_NOT_FOUND;

    *pPageIndex = index;
    return NV_OK;
}

/*
 * osCreateMemFromOsDescriptor (osmemdesc.c on Linux), of every descriptor
 * type only two:
 *
 * - NVOS32_DESCRIPTOR_TYPE_OS_PAGE_ARRAY, for a user client: the pages
 *   behind a range of its own memory, which os_lock_user_pages pinned for
 *   RM_ALLOC_MEMORY of an OS descriptor (eclipse_rm_frontend.c, "OS
 *   descriptors").
 * - NVOS32_DESCRIPTOR_TYPE_OS_PHYS_ADDR, only for a kernel-privileged
 *   caller, and only of pages /dev/nvidia-uvm lends the RM for the length
 *   of one of its calls: a semaphore pool's, which the RM then maps into a
 *   GPU VA space (eclipse_rm_frontend.c, "UVM").
 *
 * Anything else is NV_ERR_NOT_SUPPORTED, the answer every descriptor got
 * before UVM needed one, so no other physical address can be made RM
 * memory.
 *
 * The page array is osCreateOsDescriptorFromPageArray and
 * osCreateMemdescFromPages, with the array itself as memData where Linux
 * wraps it in an nv_alloc_t, and its pages' physical addresses read through
 * eclipse_user_pages_phys. The memdesc keeps the pages pinned until the RM
 * destroys it, which unlocks them, as osDestroyOsDescriptorPageArray does.
 * The pages are not one block: osMapSystemMemory maps a range of them only
 * where it is physically consecutive, for the kernel, and never for a user,
 * who has them mapped already.
 *
 * This is osCreateOsDescriptorFromPhysAddr without nv_register_phys_pages:
 * the pages are one of Eclipse's contiguous blocks, already in the physmap,
 * so osMapSystemMemory finds them through the same EclipseSysmemAlloc as
 * the RM's own. The memdesc holds the pages (eclipse_uvm_pages_hold) until
 * the RM destroys it, after every object and mapping made of it is gone,
 * however long that outlives the pool that lent them.
 */

/* nvidia-rm-sys src/frontend/uvm.rs, #[no_mangle]. */
extern void *eclipse_uvm_pages_hold(NvU64 physBase, NvU64 size);
extern void  eclipse_uvm_pages_release(void *pHold);

/* memData of a memdesc of lent pages: EclipseSysmemAlloc first, as
 * osMapSystemMemory reads every system-memory memdesc, then the hold. */
typedef struct
{
    EclipseSysmemAlloc alloc;
    void              *pHold;
} EclipseLentPages;

static void _eclipseLentPagesRelease(MEMORY_DESCRIPTOR *pMemDesc)
{
    EclipseLentPages *pLent = (EclipseLentPages *) memdescGetMemData(pMemDesc);

    memdescUnmapIommu(pMemDesc, pMemDesc->pGpu->busInfo.iovaspaceId);
    eclipse_uvm_pages_release(pLent->pHold);
    portMemFree(pLent);
}

/* nvidia-rm-sys src/frontend/user_pages.rs, #[no_mangle]: how many pages
 * an array os_lock_user_pages handed out holds, and the physical address of
 * one of them. */
extern NvU64 eclipse_user_pages_count(void *pPageArray);
extern NvU64 eclipse_user_pages_phys(void *pPageArray, NvU64 index);

/* osDestroyOsDescriptorPageArray. Like it, this leaves the IOMMU mappings
 * to memdescDestroy (bug 1811006 there). */
static void _eclipseUserPagesRelease(MEMORY_DESCRIPTOR *pMemDesc)
{
    void  *pPageArray = memdescGetMemData(pMemDesc);
    NvBool writable   = !memdescGetFlag(pMemDesc, MEMDESC_FLAGS_USER_READ_ONLY);
    NvU32  flags      = DRF_NUM(_LOCK_USER_PAGES, _FLAGS, _WRITE, writable);

    (void) os_unlock_user_pages(pMemDesc->PageCount, pPageArray, flags);
}

/* Whether the memData of `pMemDesc` is a page array: exactly while the
 * teardown that unlocks one is wired up. */
static NvBool _eclipseIsUserPages(MEMORY_DESCRIPTOR *pMemDesc)
{
    return pMemDesc->_pMemDataReleaseCallback == _eclipseUserPagesRelease;
}

/* osMapSystemMemory of `length` bytes at `offset` of a client's pages. */
static NV_STATUS _eclipseMapUserPages(
    MEMORY_DESCRIPTOR *pRoot,
    NvU64              offset,
    NvU64              length,
    NvBool             Kernel,
    NvP64             *ppAddress
)
{
    NvU64 first = offset / RM_PAGE_SIZE;
    NvU64 last  = (length == 0) ? first : (offset + length - 1) / RM_PAGE_SIZE;
    NvU64 base, va, i;

    if (!Kernel)
        return NV_ERR_NOT_SUPPORTED;

    base = memdescGetPte(pRoot, AT_CPU, (NvU32) first);
    for (i = first + 1; i <= last; i++)
    {
        if (memdescGetPte(pRoot, AT_CPU, (NvU32) i) != base + (i - first) * RM_PAGE_SIZE)
            return NV_ERR_NOT_SUPPORTED;
    }

    va = drivers_phys_to_virt(base + (offset % RM_PAGE_SIZE));
    if (va == 0)
        return NV_ERR_GENERIC;

    *ppAddress = NV_PTR_TO_NvP64((void *) (NvUPtr) va);
    return NV_OK;
}

/* osCreateOsDescriptorFromPageArray + osCreateMemdescFromPages. On failure
 * the pages stay locked: the caller unlocks them, as on Linux. */
static NV_STATUS _eclipseCreateFromPageArray(
    OBJGPU             *pGpu,
    NvP64               pDescriptor,
    NvU32               flags,
    NvU64              *pLimit,
    MEMORY_DESCRIPTOR **ppMemDesc
)
{
    void              *pPageArray   = NvP64_VALUE(pDescriptor);
    NvU64              memdescFlags = MEMDESC_FLAGS_NONE;
    NvU64              size, pageCount, i;
    NvU64             *pPteArray;
    MEMORY_DESCRIPTOR *pMemDesc;
    NvU32              gpuCachedFlags;
    NV_STATUS          status;

    //
    // Since the only type of memory permitted in this path
    // is anonymous user memory, certain restrictions
    // apply for the allocation flags:
    //
    //  1) anonymous memory is write-back cacheable, hence
    //     the _COHERENCY flag must match.
    //
    //  2) the RM has no control over the location of the
    //     associated pages in memory and thus cannot
    //     honor requests for contiguous memory.
    //
    if ((!FLD_TEST_DRF(OS02, _FLAGS, _COHERENCY, _CACHED, flags) &&
         !FLD_TEST_DRF(OS02, _FLAGS, _COHERENCY, _WRITE_BACK, flags)) ||
         FLD_TEST_DRF(OS02, _FLAGS, _PHYSICALITY, _CONTIGUOUS, flags))
    {
        return NV_ERR_INVALID_FLAGS;
    }

    if ((*pLimit + 1) == 0)
        return NV_ERR_INVALID_ARGUMENT;
    size      = NV_ALIGN_UP64(*pLimit + 1, RM_PAGE_SIZE);
    pageCount = size / RM_PAGE_SIZE;

    /* What os_lock_user_pages pinned is exactly this range's pages. */
    if ((size == 0) || (eclipse_user_pages_count(pPageArray) != pageCount))
        return NV_ERR_INVALID_ARGUMENT;

    if (FLD_TEST_DRF(OS02, _FLAGS, _ALLOC_NISO_DISPLAY, _YES, flags))
        memdescFlags |= MEMDESC_FLAGS_MEMORY_TYPE_DISPLAY_NISO;

    if (FLD_TEST_DRF(OS02, _FLAGS, _MEMORY_PROTECTION, _UNPROTECTED, flags))
        memdescFlags |= MEMDESC_FLAGS_ALLOC_IN_UNPROTECTED_MEMORY;

    if (FLD_TEST_DRF(OS02, _FLAGS, _ALLOC_USER_READ_ONLY, _YES, flags))
    {
        memdescFlags |= MEMDESC_FLAGS_USER_READ_ONLY;
        memdescFlags |= MEMDESC_FLAGS_DEVICE_READ_ONLY;
    }

    if (FLD_TEST_DRF(OS02, _FLAGS, _ALLOC_DEVICE_READ_ONLY, _YES, flags))
        memdescFlags |= MEMDESC_FLAGS_DEVICE_READ_ONLY;

    status = memdescCreate(ppMemDesc, pGpu, size, 0, NV_MEMORY_NONCONTIGUOUS,
                           ADDR_SYSMEM, NV_MEMORY_CACHED, memdescFlags);
    if (status != NV_OK)
        return status;

    if (FLD_TEST_DRF(OS02, _FLAGS, _GPU_CACHEABLE, _YES, flags))
        gpuCachedFlags = NV_MEMORY_CACHED;
    else
        gpuCachedFlags = NV_MEMORY_UNCACHED;

    /* nv_register_user_pages: each page's physical address. */
    pMemDesc  = *ppMemDesc;
    pPteArray = memdescGetPteArray(pMemDesc, AT_CPU);
    for (i = 0; i < pageCount; i++)
        pPteArray[i] = eclipse_user_pages_phys(pPageArray, i);

    memdescSetGpuCacheAttrib(pMemDesc, gpuCachedFlags);
    memdescSetAddress(pMemDesc, NvP64_NULL);
    memdescSetFlag(pMemDesc, MEMDESC_FLAGS_KERNEL_MODE, NV_FALSE);
    memdescSetFlag(pMemDesc, MEMDESC_FLAGS_EXT_PAGE_ARRAY_MEM, NV_TRUE);

    if (!NV_IS_ALIGNED64(memdescGetPhysAddr(pMemDesc, AT_CPU, 0), RM_PAGE_SIZE))
        status = NV_ERR_INVALID_ARGUMENT;
    else
        status = memdescSetAllocSizeFields(pMemDesc, size, RM_PAGE_SIZE);

    if (status == NV_OK)
    {
        /* As on Linux, the private data is set, without its teardown, for
         * memdescMapIommu, and the teardown wired up once that worked. */
        memdescSetMemData(pMemDesc, pPageArray, NULL);
        status = memdescMapIommu(pMemDesc, pGpu->busInfo.iovaspaceId);
    }
    if (status != NV_OK)
    {
        memdescSetMemData(pMemDesc, NULL, NULL);
        memdescDestroy(pMemDesc);
        *ppMemDesc = NULL;
        return status;
    }

    memdescSetMemData(pMemDesc, pPageArray, _eclipseUserPagesRelease);
    return NV_OK;
}

NV_STATUS osCreateMemFromOsDescriptor(
    OBJGPU             *pGpu,
    NvP64               pDescriptor,
    NvHandle            hClient,
    NvU32               flags,
    NvU64              *pLimit,
    MEMORY_DESCRIPTOR **ppMemDesc,
    NvU32               descriptorType,
    RS_PRIV_LEVEL       privilegeLevel
)
{
    NvU64              base = (NvU64) (NvUPtr) NvP64_VALUE(pDescriptor);
    NvU64              size;
    MEMORY_DESCRIPTOR *pMemDesc;
    EclipseLentPages  *pLent;
    NV_STATUS          status;

    if (descriptorType == NVOS32_DESCRIPTOR_TYPE_OS_PAGE_ARRAY)
    {
        RmClient *pClient = serverutilGetClientUnderLock(hClient);

        if ((pDescriptor == NvP64_NULL) || (pLimit == NULL) || (*pLimit == 0) ||
            (ppMemDesc == NULL) || (pClient == NULL))
            return NV_ERR_INVALID_PARAM_STRUCT;

        /* As on Linux: a kernel client has no user pages to lend. */
        if (rmclientGetCachedPrivilege(pClient) >= RS_PRIV_LEVEL_KERNEL)
            return NV_ERR_NOT_SUPPORTED;

        return _eclipseCreateFromPageArray(pGpu, pDescriptor, flags, pLimit,
                                           ppMemDesc);
    }

    if ((descriptorType != NVOS32_DESCRIPTOR_TYPE_OS_PHYS_ADDR) ||
        (privilegeLevel < RS_PRIV_LEVEL_KERNEL))
        return NV_ERR_NOT_SUPPORTED;

    if ((base == 0) || (pLimit == NULL) || (*pLimit == 0) || (ppMemDesc == NULL))
        return NV_ERR_INVALID_PARAM_STRUCT;

    /* osCreateOsDescriptorFromPhysAddr: contiguous memory only. */
    if (!FLD_TEST_DRF(OS02, _FLAGS, _PHYSICALITY, _CONTIGUOUS, flags))
        return NV_ERR_INVALID_FLAGS;

    if (!NV_IS_ALIGNED64(base, RM_PAGE_SIZE) || ((*pLimit + 1) == 0))
        return NV_ERR_INVALID_ARGUMENT;
    size = NV_ALIGN_UP64(*pLimit + 1, RM_PAGE_SIZE);

    pLent = portMemAllocNonPaged(sizeof(*pLent));
    if (pLent == NULL)
        return NV_ERR_NO_MEMORY;

    pLent->pHold = eclipse_uvm_pages_hold(base, size);
    if (pLent->pHold == NULL)
    {
        portMemFree(pLent);
        return NV_ERR_NOT_SUPPORTED;
    }
    pLent->alloc.physBase  = base;
    pLent->alloc.pageCount = size / RM_PAGE_SIZE;
    pLent->alloc.cacheType = NV_MEMORY_CACHED;

    status = memdescCreate(ppMemDesc, pGpu, size, 0, NV_MEMORY_CONTIGUOUS,
                           ADDR_SYSMEM, NV_MEMORY_CACHED, MEMDESC_FLAGS_NONE);
    if (status != NV_OK)
        goto release;

    pMemDesc = *ppMemDesc;
    memdescSetAddress(pMemDesc, NvP64_NULL);
    memdescSetFlag(pMemDesc, MEMDESC_FLAGS_EXT_PAGE_ARRAY_MEM, NV_TRUE);
    memdescGetPteArray(pMemDesc, AT_CPU)[0] = base;
    memdescSetGpuCacheAttrib(pMemDesc,
        FLD_TEST_DRF(OS02, _FLAGS, _GPU_CACHEABLE, _YES, flags) ?
            NV_MEMORY_CACHED : NV_MEMORY_UNCACHED);

    status = memdescSetAllocSizeFields(pMemDesc, size, RM_PAGE_SIZE);
    if (status == NV_OK)
    {
        /* As on Linux, the private data is set, without its teardown, for
         * memdescMapIommu, and the teardown wired up once that worked. */
        memdescSetMemData(pMemDesc, pLent, NULL);
        status = memdescMapIommu(pMemDesc, pGpu->busInfo.iovaspaceId);
    }
    if (status != NV_OK)
    {
        memdescSetMemData(pMemDesc, NULL, NULL);
        memdescDestroy(pMemDesc);
        *ppMemDesc = NULL;
        goto release;
    }

    memdescSetMemData(pMemDesc, pLent, _eclipseLentPagesRelease);
    return NV_OK;

release:
    eclipse_uvm_pages_release(pLent->pHold);
    portMemFree(pLent);
    return status;
}

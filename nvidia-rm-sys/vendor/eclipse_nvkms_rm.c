/*
 * The RM half of nvidia-modeset's way into the RM.
 *
 * In Linux nvkms reaches the RM through nvidia_modeset_rm_ops_t
 * (kernel-open/nvidia/nv-modeset-interface.c): `op` is rm_kernel_rmapi_op in
 * arch/nvalloc/unix/src/osapi.c, and `enumerate_gpus` / `open_gpu` walk the
 * Linux device list. That whole directory is NVIDIA's Linux platform layer,
 * which build.rs leaves out, so the pieces nvkms needs are here instead. The
 * Rust hooks in src/nvkms_os.rs call them with the RM gate held.
 */

#include <nv.h>
#include <os/os.h>
#include "gpu/gpu.h"
#include <gpu_mgr/gpu_mgr.h>
#include "rmapi/exports.h"
#include <core/locks.h>
#include <nv-kernel-rmapi-ops.h>
#include <nv-gpu-info.h>
#include "tls/tls.h"

/*
 * rm_kernel_rmapi_op, verbatim apart from NV_ENTER/EXIT_RM_RUNTIME (the
 * stack switch Linux needs and this kernel does not). Every case is an
 * RM-core *Kernel entry point that build.rs already compiles.
 */
void eclipse_rm_kernel_rmapi_op(void *ops_cmd)
{
    nvidia_kernel_rmapi_ops_t *ops = ops_cmd;

    switch (ops->op)
    {
        case NV01_FREE:
            Nv01FreeKernel(&ops->params.free);
            break;

        case NV01_ALLOC_MEMORY:
            Nv01AllocMemoryKernel(&ops->params.allocMemory64);
            break;

        case NV04_ALLOC:
            Nv04AllocWithAccessKernel(&ops->params.alloc);
            break;

        case NV04_VID_HEAP_CONTROL:
            Nv04VidHeapControlKernel(ops->params.pVidHeapControl);
            break;

        case NV04_MAP_MEMORY:
        {
            //
            // We need to free NVUAP for MEM_SPACE_USER mappings,
            // since we're not going through the regular path
            //
            nv_usermap_access_params_t *pNvuap = NULL;
            Nv04MapMemoryKernel(&ops->params.mapMemory);
            pNvuap = (nv_usermap_access_params_t *) tlsEntryGet(TLS_ENTRY_ID_MAPPING_CONTEXT);
            if (pNvuap != NULL)
            {
                NV_ASSERT(tlsEntryRelease(TLS_ENTRY_ID_MAPPING_CONTEXT) == 0);
                os_free_mem(pNvuap->memArea.pRanges);
                os_free_mem(pNvuap);
            }
            break;
        }

        case NV04_UNMAP_MEMORY:
            Nv04UnmapMemoryKernel(&ops->params.unmapMemory);
            break;

        case NV04_ALLOC_CONTEXT_DMA:
            Nv04AllocContextDmaKernel(&ops->params.allocContextDma2);
            break;

        case NV04_MAP_MEMORY_DMA:
            Nv04MapMemoryDmaKernel(&ops->params.mapMemoryDma);
            break;

        case NV04_UNMAP_MEMORY_DMA:
            Nv04UnmapMemoryDmaKernel(&ops->params.unmapMemoryDma);
            break;

        case NV04_BIND_CONTEXT_DMA:
            Nv04BindContextDmaKernel(&ops->params.bindContextDma);
            break;

        case NV04_CONTROL:
            Nv04ControlKernel(&ops->params.control);
            break;

        case NV04_DUP_OBJECT:
            Nv04DupObjectKernel(&ops->params.dupObject);
            break;

        case NV04_SHARE:
            Nv04ShareKernel(&ops->params.share);
            break;

        case NV04_ADD_VBLANK_CALLBACK:
            Nv04AddVblankCallbackKernel(&ops->params.addVblankCallback);
            break;
    }

}

/*
 * nvidia_modeset_enumerate_gpus: every GPU the RM has attached, with the gpu
 * id the RM gave it (gpuGenerate32BitId of its domain/bus/device, the same
 * value Linux keeps in nv->gpu_id). `gpu_info` has NV_MAX_GPUS entries.
 */
NvU32 eclipse_rm_enumerate_gpus(nv_gpu_info_t *gpu_info)
{
    NvU32 gpuCount = 0, gpuMask = 0, index = 0, count = 0;
    OBJGPU *pGpu;

    if (gpumgrGetGpuAttachInfo(&gpuCount, &gpuMask) != NV_OK)
        return 0;
    while ((pGpu = gpumgrGetNextGpu(gpuMask, &index)) != NULL) {
        if (count >= NV_MAX_GPUS)
            break;
        portMemSet(&gpu_info[count], 0, sizeof(gpu_info[count]));
        gpu_info[count].gpu_id = pGpu->gpuId;
        gpu_info[count].pci_info.domain = gpuGetDomain(pGpu);
        gpu_info[count].pci_info.bus = gpuGetBus(pGpu);
        gpu_info[count].pci_info.slot = gpuGetDevice(pGpu);
        gpu_info[count].pci_info.function = 0;
        count++;
    }
    return count;
}

/* nvidia_modeset_open_gpu's question, minus the bring-up: is this GPU attached? */
NvBool eclipse_rm_gpu_id_attached(NvU32 gpuId)
{
    return gpumgrGetGpuFromId(gpuId) != NULL;
}

// ecl-vkcompute — SAXPY via Vulkan compute (NVK / any ICD).
//
// This is the userspace counterpart of `ecl-compute saxpy`: same math, but
// through Mesa NVK (`vkCmdDispatch`) instead of the kernel ioctl. card0 is
// the nouveau node NVK talks to; card1 (`eclipse-compute`) is skipped by
// Mesa on purpose.
//
// No Vulkan SDK at build time: types are declared here and libvulkan.so.1
// is loaded at runtime (dlopen). Static musl cannot dlopen, so this binary
// is dynamic.
//
// Usage: ecl-vkcompute [n]
//   n = number of floats (default 1024, must be a multiple of 32).

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "saxpy_spv.h"

typedef struct VkInstance_T *VkInstance;
typedef struct VkPhysicalDevice_T *VkPhysicalDevice;
typedef struct VkDevice_T *VkDevice;
typedef struct VkQueue_T *VkQueue;
typedef struct VkCommandBuffer_T *VkCommandBuffer;
typedef uint64_t VkNonDisp;
typedef VkNonDisp VkBuffer;
typedef VkNonDisp VkDeviceMemory;
typedef VkNonDisp VkShaderModule;
typedef VkNonDisp VkDescriptorSetLayout;
typedef VkNonDisp VkPipelineLayout;
typedef VkNonDisp VkPipeline;
typedef VkNonDisp VkDescriptorPool;
typedef VkNonDisp VkDescriptorSet;
typedef VkNonDisp VkCommandPool;
typedef VkNonDisp VkFence;
typedef VkNonDisp VkPipelineCache;

typedef int32_t VkResult;
typedef uint32_t VkFlags;
typedef uint64_t VkDeviceSize;
typedef void (*PFN_vkVoidFunction)(void);

#define VK_SUCCESS 0
#define VK_ERROR_DEVICE_LOST (-4)
#define VK_WHOLE_SIZE (~(VkDeviceSize)0)
#define VK_QUEUE_GRAPHICS_BIT 0x00000001u
#define VK_QUEUE_COMPUTE_BIT 0x00000002u
#define VK_BUFFER_USAGE_STORAGE_BUFFER_BIT 0x00000020u
#define VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT 0x00000002u
#define VK_MEMORY_PROPERTY_HOST_COHERENT_BIT 0x00000004u
#define VK_DESCRIPTOR_TYPE_STORAGE_BUFFER 7
#define VK_SHADER_STAGE_COMPUTE_BIT 0x00000020u
#define VK_PIPELINE_BIND_POINT_COMPUTE 1
#define VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU 2
#define VK_SHARING_MODE_EXCLUSIVE 0
#define VK_COMMAND_BUFFER_LEVEL_PRIMARY 0
#define VK_TRUE 1

#define ST_APPLICATION_INFO 0
#define ST_INSTANCE_CREATE_INFO 1
#define ST_DEVICE_QUEUE_CREATE_INFO 2
#define ST_DEVICE_CREATE_INFO 3
#define ST_SUBMIT_INFO 4
#define ST_MEMORY_ALLOCATE_INFO 5
#define ST_BUFFER_CREATE_INFO 12
#define ST_SHADER_MODULE_CREATE_INFO 16
#define ST_PIPELINE_SHADER_STAGE_CREATE_INFO 18
#define ST_COMPUTE_PIPELINE_CREATE_INFO 29
#define ST_PIPELINE_LAYOUT_CREATE_INFO 30
#define ST_DESCRIPTOR_SET_LAYOUT_CREATE_INFO 32
#define ST_DESCRIPTOR_POOL_CREATE_INFO 33
#define ST_DESCRIPTOR_SET_ALLOCATE_INFO 34
#define ST_WRITE_DESCRIPTOR_SET 35
#define ST_COMMAND_POOL_CREATE_INFO 39
#define ST_COMMAND_BUFFER_ALLOCATE_INFO 40
#define ST_COMMAND_BUFFER_BEGIN_INFO 42

#define VK_API_VERSION_1_0 ((1u << 22) | (0u << 12))

typedef struct {
    int32_t sType;
    const void *pNext;
    const char *pApplicationName;
    uint32_t applicationVersion;
    const char *pEngineName;
    uint32_t engineVersion;
    uint32_t apiVersion;
} VkApplicationInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    const VkApplicationInfo *pApplicationInfo;
    uint32_t enabledLayerCount;
    const char *const *ppEnabledLayerNames;
    uint32_t enabledExtensionCount;
    const char *const *ppEnabledExtensionNames;
} VkInstanceCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t queueFamilyIndex;
    uint32_t queueCount;
    const float *pQueuePriorities;
} VkDeviceQueueCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t queueCreateInfoCount;
    const VkDeviceQueueCreateInfo *pQueueCreateInfos;
    uint32_t enabledLayerCount;
    const char *const *ppEnabledLayerNames;
    uint32_t enabledExtensionCount;
    const char *const *ppEnabledExtensionNames;
    const void *pEnabledFeatures;
} VkDeviceCreateInfo;

typedef struct {
    uint32_t apiVersion, driverVersion, vendorID, deviceID, deviceType;
    char deviceName[256];
    uint8_t pipelineCacheUUID[16];
    uint8_t rest[2048];
} VkPhysicalDeviceProperties;

typedef struct {
    uint32_t propertyFlags;
    uint32_t heapIndex;
} VkMemoryType;
typedef struct {
    VkDeviceSize size;
    VkFlags flags;
} VkMemoryHeap;
typedef struct {
    uint32_t memoryTypeCount;
    VkMemoryType memoryTypes[32];
    uint32_t memoryHeapCount;
    VkMemoryHeap memoryHeaps[16];
} VkPhysicalDeviceMemoryProperties;

typedef struct {
    uint32_t width, height, depth;
} VkExtent3D;
typedef struct {
    VkFlags queueFlags;
    uint32_t queueCount;
    uint32_t timestampValidBits;
    VkExtent3D minImageTransferGranularity;
} VkQueueFamilyProperties;

typedef struct {
    VkDeviceSize size;
    VkDeviceSize alignment;
    uint32_t memoryTypeBits;
} VkMemoryRequirements;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    VkDeviceSize size;
    VkFlags usage;
    uint32_t sharingMode;
    uint32_t queueFamilyIndexCount;
    const uint32_t *pQueueFamilyIndices;
} VkBufferCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkDeviceSize allocationSize;
    uint32_t memoryTypeIndex;
} VkMemoryAllocateInfo;

typedef struct {
    uint32_t binding;
    uint32_t descriptorType;
    uint32_t descriptorCount;
    VkFlags stageFlags;
    const void *pImmutableSamplers;
} VkDescriptorSetLayoutBinding;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t bindingCount;
    const VkDescriptorSetLayoutBinding *pBindings;
} VkDescriptorSetLayoutCreateInfo;

typedef struct {
    VkFlags stageFlags;
    uint32_t offset;
    uint32_t size;
} VkPushConstantRange;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t setLayoutCount;
    const VkDescriptorSetLayout *pSetLayouts;
    uint32_t pushConstantRangeCount;
    const VkPushConstantRange *pPushConstantRanges;
} VkPipelineLayoutCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    size_t codeSize;
    const uint32_t *pCode;
} VkShaderModuleCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    VkFlags stage;
    VkShaderModule module;
    const char *pName;
    const void *pSpecializationInfo;
} VkPipelineShaderStageCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    VkPipelineShaderStageCreateInfo stage;
    VkPipelineLayout layout;
    VkPipeline basePipelineHandle;
    int32_t basePipelineIndex;
} VkComputePipelineCreateInfo;

typedef struct {
    uint32_t type;
    uint32_t descriptorCount;
} VkDescriptorPoolSize;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t maxSets;
    uint32_t poolSizeCount;
    const VkDescriptorPoolSize *pPoolSizes;
} VkDescriptorPoolCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkDescriptorPool descriptorPool;
    uint32_t descriptorSetCount;
    const VkDescriptorSetLayout *pSetLayouts;
} VkDescriptorSetAllocateInfo;

typedef struct {
    VkBuffer buffer;
    VkDeviceSize offset;
    VkDeviceSize range;
} VkDescriptorBufferInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkDescriptorSet dstSet;
    uint32_t dstBinding;
    uint32_t dstArrayElement;
    uint32_t descriptorCount;
    uint32_t descriptorType;
    const void *pImageInfo;
    const VkDescriptorBufferInfo *pBufferInfo;
    const void *pTexelBufferView;
} VkWriteDescriptorSet;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    uint32_t queueFamilyIndex;
} VkCommandPoolCreateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkCommandPool commandPool;
    uint32_t level;
    uint32_t commandBufferCount;
} VkCommandBufferAllocateInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    VkFlags flags;
    const void *pInheritanceInfo;
} VkCommandBufferBeginInfo;

typedef struct {
    int32_t sType;
    const void *pNext;
    uint32_t waitSemaphoreCount;
    const VkNonDisp *pWaitSemaphores;
    const VkFlags *pWaitDstStageMask;
    uint32_t commandBufferCount;
    const VkCommandBuffer *pCommandBuffers;
    uint32_t signalSemaphoreCount;
    const VkNonDisp *pSignalSemaphores;
} VkSubmitInfo;

#define PFNS \
    X(vkDestroyInstance, void, (VkInstance, const void *)) \
    X(vkEnumeratePhysicalDevices, VkResult, (VkInstance, uint32_t *, VkPhysicalDevice *)) \
    X(vkGetPhysicalDeviceProperties, void, (VkPhysicalDevice, VkPhysicalDeviceProperties *)) \
    X(vkGetPhysicalDeviceMemoryProperties, void, (VkPhysicalDevice, VkPhysicalDeviceMemoryProperties *)) \
    X(vkGetPhysicalDeviceQueueFamilyProperties, void, (VkPhysicalDevice, uint32_t *, VkQueueFamilyProperties *)) \
    X(vkCreateDevice, VkResult, (VkPhysicalDevice, const VkDeviceCreateInfo *, const void *, VkDevice *)) \
    X(vkDestroyDevice, void, (VkDevice, const void *)) \
    X(vkGetDeviceQueue, void, (VkDevice, uint32_t, uint32_t, VkQueue *)) \
    X(vkCreateBuffer, VkResult, (VkDevice, const VkBufferCreateInfo *, const void *, VkBuffer *)) \
    X(vkDestroyBuffer, void, (VkDevice, VkBuffer, const void *)) \
    X(vkGetBufferMemoryRequirements, void, (VkDevice, VkBuffer, VkMemoryRequirements *)) \
    X(vkAllocateMemory, VkResult, (VkDevice, const VkMemoryAllocateInfo *, const void *, VkDeviceMemory *)) \
    X(vkFreeMemory, void, (VkDevice, VkDeviceMemory, const void *)) \
    X(vkBindBufferMemory, VkResult, (VkDevice, VkBuffer, VkDeviceMemory, VkDeviceSize)) \
    X(vkMapMemory, VkResult, (VkDevice, VkDeviceMemory, VkDeviceSize, VkDeviceSize, VkFlags, void **)) \
    X(vkUnmapMemory, void, (VkDevice, VkDeviceMemory)) \
    X(vkCreateShaderModule, VkResult, (VkDevice, const VkShaderModuleCreateInfo *, const void *, VkShaderModule *)) \
    X(vkDestroyShaderModule, void, (VkDevice, VkShaderModule, const void *)) \
    X(vkCreateDescriptorSetLayout, VkResult, (VkDevice, const VkDescriptorSetLayoutCreateInfo *, const void *, VkDescriptorSetLayout *)) \
    X(vkDestroyDescriptorSetLayout, void, (VkDevice, VkDescriptorSetLayout, const void *)) \
    X(vkCreatePipelineLayout, VkResult, (VkDevice, const VkPipelineLayoutCreateInfo *, const void *, VkPipelineLayout *)) \
    X(vkDestroyPipelineLayout, void, (VkDevice, VkPipelineLayout, const void *)) \
    X(vkCreateComputePipelines, VkResult, (VkDevice, VkPipelineCache, uint32_t, const VkComputePipelineCreateInfo *, const void *, VkPipeline *)) \
    X(vkDestroyPipeline, void, (VkDevice, VkPipeline, const void *)) \
    X(vkCreateDescriptorPool, VkResult, (VkDevice, const VkDescriptorPoolCreateInfo *, const void *, VkDescriptorPool *)) \
    X(vkDestroyDescriptorPool, void, (VkDevice, VkDescriptorPool, const void *)) \
    X(vkAllocateDescriptorSets, VkResult, (VkDevice, const VkDescriptorSetAllocateInfo *, VkDescriptorSet *)) \
    X(vkUpdateDescriptorSets, void, (VkDevice, uint32_t, const VkWriteDescriptorSet *, uint32_t, const void *)) \
    X(vkCreateCommandPool, VkResult, (VkDevice, const VkCommandPoolCreateInfo *, const void *, VkCommandPool *)) \
    X(vkDestroyCommandPool, void, (VkDevice, VkCommandPool, const void *)) \
    X(vkAllocateCommandBuffers, VkResult, (VkDevice, const VkCommandBufferAllocateInfo *, VkCommandBuffer *)) \
    X(vkBeginCommandBuffer, VkResult, (VkCommandBuffer, const VkCommandBufferBeginInfo *)) \
    X(vkCmdBindPipeline, void, (VkCommandBuffer, uint32_t, VkPipeline)) \
    X(vkCmdBindDescriptorSets, void, (VkCommandBuffer, uint32_t, VkPipelineLayout, uint32_t, uint32_t, const VkDescriptorSet *, uint32_t, const uint32_t *)) \
    X(vkCmdPushConstants, void, (VkCommandBuffer, VkPipelineLayout, VkFlags, uint32_t, uint32_t, const void *)) \
    X(vkCmdDispatch, void, (VkCommandBuffer, uint32_t, uint32_t, uint32_t)) \
    X(vkEndCommandBuffer, VkResult, (VkCommandBuffer)) \
    X(vkQueueSubmit, VkResult, (VkQueue, uint32_t, const VkSubmitInfo *, VkFence)) \
    X(vkQueueWaitIdle, VkResult, (VkQueue))

#define X(name, ret, args) static ret(*name) args;
PFNS
#undef X

static PFN_vkVoidFunction (*vkGetInstanceProcAddr)(VkInstance, const char *);
static VkResult (*vkCreateInstance)(const VkInstanceCreateInfo *, const void *, VkInstance *);

static const char *vk_err_str(VkResult r) {
    if (r == VK_SUCCESS)
        return "VK_SUCCESS";
    if (r == VK_ERROR_DEVICE_LOST)
        return "VK_ERROR_DEVICE_LOST";
    static char buf[32];
    snprintf(buf, sizeof(buf), "VkResult %d", (int)r);
    return buf;
}

static int fail(const char *what, VkResult r) {
    fprintf(stderr, "ecl-vkcompute: %s: %s\n", what, vk_err_str(r));
    if (r == VK_ERROR_DEVICE_LOST)
        fprintf(stderr,
                "ecl-vkcompute: DEVICE_LOST usually means EXEC/SYNCOBJ on card0 failed;\n"
                "  try `ecl-compute saxpy` (kernel canary) and `dmesg | grep nouveau-uapi`.\n");
    return 1;
}

static uint32_t pick_mem(const VkPhysicalDeviceMemoryProperties *mp, uint32_t bits, uint32_t want) {
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want)
            return i;
    }
    return ~0u;
}

static int make_ssbo(VkDevice dev, const VkPhysicalDeviceMemoryProperties *mp, VkDeviceSize size,
                     VkBuffer *buf, VkDeviceMemory *mem, void **mapped) {
    VkBufferCreateInfo bi = {
        .sType = ST_BUFFER_CREATE_INFO,
        .size = size,
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkResult r = vkCreateBuffer(dev, &bi, NULL, buf);
    if (r != VK_SUCCESS)
        return fail("vkCreateBuffer", r);
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(dev, *buf, &req);
    uint32_t idx = pick_mem(mp, req.memoryTypeBits,
                            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if (idx == ~0u) {
        fprintf(stderr, "ecl-vkcompute: no HOST_VISIBLE|COHERENT memory type\n");
        return 1;
    }
    VkMemoryAllocateInfo ai = {
        .sType = ST_MEMORY_ALLOCATE_INFO,
        .allocationSize = req.size,
        .memoryTypeIndex = idx,
    };
    r = vkAllocateMemory(dev, &ai, NULL, mem);
    if (r != VK_SUCCESS)
        return fail("vkAllocateMemory", r);
    r = vkBindBufferMemory(dev, *buf, *mem, 0);
    if (r != VK_SUCCESS)
        return fail("vkBindBufferMemory", r);
    r = vkMapMemory(dev, *mem, 0, VK_WHOLE_SIZE, 0, mapped);
    if (r != VK_SUCCESS)
        return fail("vkMapMemory", r);
    return 0;
}

int main(int argc, char **argv) {
    uint32_t n = 1024;
    if (argc > 1) {
        char *end = NULL;
        unsigned long v = strtoul(argv[1], &end, 10);
        if (!end || *end || v == 0 || v % 32 != 0 || v > 16u * 1024u * 1024u) {
            fprintf(stderr, "ecl-vkcompute: n must be a positive multiple of 32 (got '%s')\n", argv[1]);
            return 2;
        }
        n = (uint32_t)v;
    }

    void *lib = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib)
        lib = dlopen("libvulkan.so", RTLD_NOW | RTLD_LOCAL);
    if (!lib) {
        fprintf(stderr, "ecl-vkcompute: dlopen libvulkan: %s\n"
                        "  (need vulkan-loader + mesa-vulkan-nouveau on the image)\n",
                dlerror());
        return 1;
    }
    vkGetInstanceProcAddr = (PFN_vkVoidFunction(*)(VkInstance, const char *))dlsym(lib, "vkGetInstanceProcAddr");
    if (!vkGetInstanceProcAddr) {
        fprintf(stderr, "ecl-vkcompute: no vkGetInstanceProcAddr\n");
        return 1;
    }
    vkCreateInstance = (VkResult(*)(const VkInstanceCreateInfo *, const void *, VkInstance *))
        vkGetInstanceProcAddr(NULL, "vkCreateInstance");
    if (!vkCreateInstance)
        return fail("vkCreateInstance lookup", -1);

    VkApplicationInfo app = {
        .sType = ST_APPLICATION_INFO,
        .pApplicationName = "ecl-vkcompute",
        .apiVersion = VK_API_VERSION_1_0,
    };
    VkInstanceCreateInfo ici = {
        .sType = ST_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst = NULL;
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    if (r != VK_SUCCESS)
        return fail("vkCreateInstance", r);

#define X(name, ret, args) \
    name = (ret(*) args)vkGetInstanceProcAddr(inst, #name); \
    if (!name) { fprintf(stderr, "ecl-vkcompute: missing %s\n", #name); return 1; }
    PFNS
#undef X

    uint32_t ndev = 0;
    r = vkEnumeratePhysicalDevices(inst, &ndev, NULL);
    if (r != VK_SUCCESS || ndev == 0) {
        fprintf(stderr, "ecl-vkcompute: no Vulkan physical devices (%s, count=%u)\n",
                vk_err_str(r), ndev);
        return 1;
    }
    VkPhysicalDevice *devs = calloc(ndev, sizeof(*devs));
    r = vkEnumeratePhysicalDevices(inst, &ndev, devs);
    if (r != VK_SUCCESS)
        return fail("vkEnumeratePhysicalDevices", r);

    VkPhysicalDevice phys = NULL;
    VkPhysicalDeviceProperties props;
    uint32_t qfam = 0;
    for (int pass = 0; pass < 2 && !phys; pass++) {
        for (uint32_t i = 0; i < ndev; i++) {
            vkGetPhysicalDeviceProperties(devs[i], &props);
            if (pass == 0 && props.deviceType != VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU)
                continue;
            uint32_t nq = 0;
            vkGetPhysicalDeviceQueueFamilyProperties(devs[i], &nq, NULL);
            VkQueueFamilyProperties *qs = calloc(nq, sizeof(*qs));
            vkGetPhysicalDeviceQueueFamilyProperties(devs[i], &nq, qs);
            int found = -1;
            for (uint32_t q = 0; q < nq; q++) {
                if (qs[q].queueFlags & VK_QUEUE_COMPUTE_BIT) {
                    found = (int)q;
                    break;
                }
            }
            free(qs);
            if (found >= 0) {
                phys = devs[i];
                qfam = (uint32_t)found;
                vkGetPhysicalDeviceProperties(phys, &props);
                break;
            }
        }
    }
    free(devs);
    if (!phys) {
        fprintf(stderr, "ecl-vkcompute: no device with a compute queue\n");
        return 1;
    }
    fprintf(stderr, "ecl-vkcompute: device='%s' type=%u queue_family=%u n=%u\n",
            props.deviceName, props.deviceType, qfam, n);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = ST_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = qfam,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };
    VkDeviceCreateInfo dci = {
        .sType = ST_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
    };
    VkDevice dev = NULL;
    r = vkCreateDevice(phys, &dci, NULL, &dev);
    if (r != VK_SUCCESS)
        return fail("vkCreateDevice", r);
    VkQueue queue = NULL;
    vkGetDeviceQueue(dev, qfam, 0, &queue);

    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(phys, &mp);

    const VkDeviceSize bytes = (VkDeviceSize)n * sizeof(float);
    VkBuffer bx = 0, by = 0, bz = 0;
    VkDeviceMemory mx = 0, my = 0, mz = 0;
    float *x = NULL, *y = NULL, *z = NULL;
    if (make_ssbo(dev, &mp, bytes, &bx, &mx, (void **)&x) ||
        make_ssbo(dev, &mp, bytes, &by, &my, (void **)&y) ||
        make_ssbo(dev, &mp, bytes, &bz, &mz, (void **)&z))
        return 1;
    const float a = 2.0f;
    for (uint32_t i = 0; i < n; i++) {
        x[i] = (float)i;
        y[i] = 1.0f;
        z[i] = 0.0f;
    }

    VkDescriptorSetLayoutBinding binds[3];
    memset(binds, 0, sizeof(binds));
    for (int i = 0; i < 3; i++) {
        binds[i].binding = (uint32_t)i;
        binds[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        binds[i].descriptorCount = 1;
        binds[i].stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
    }
    VkDescriptorSetLayoutCreateInfo dslci = {
        .sType = ST_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 3,
        .pBindings = binds,
    };
    VkDescriptorSetLayout dsl = 0;
    r = vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl);
    if (r != VK_SUCCESS)
        return fail("vkCreateDescriptorSetLayout", r);
    VkPushConstantRange pcr = {
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        .offset = 0,
        .size = 4,
    };
    VkPipelineLayoutCreateInfo plci = {
        .sType = ST_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &dsl,
        .pushConstantRangeCount = 1,
        .pPushConstantRanges = &pcr,
    };
    VkPipelineLayout layout = 0;
    r = vkCreatePipelineLayout(dev, &plci, NULL, &layout);
    if (r != VK_SUCCESS)
        return fail("vkCreatePipelineLayout", r);

    VkShaderModuleCreateInfo smci = {
        .sType = ST_SHADER_MODULE_CREATE_INFO,
        .codeSize = sizeof(k_saxpy_spv),
        .pCode = (const uint32_t *)k_saxpy_spv,
    };
    VkShaderModule sm = 0;
    r = vkCreateShaderModule(dev, &smci, NULL, &sm);
    if (r != VK_SUCCESS)
        return fail("vkCreateShaderModule", r);

    VkComputePipelineCreateInfo cpci = {
        .sType = ST_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {
            .sType = ST_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT,
            .module = sm,
            .pName = "main",
        },
        .layout = layout,
        .basePipelineIndex = -1,
    };
    VkPipeline pipe = 0;
    r = vkCreateComputePipelines(dev, 0, 1, &cpci, NULL, &pipe);
    if (r != VK_SUCCESS)
        return fail("vkCreateComputePipelines", r);

    VkDescriptorPoolSize poolsz = {.type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 3};
    VkDescriptorPoolCreateInfo dpci = {
        .sType = ST_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1,
        .poolSizeCount = 1,
        .pPoolSizes = &poolsz,
    };
    VkDescriptorPool pool = 0;
    r = vkCreateDescriptorPool(dev, &dpci, NULL, &pool);
    if (r != VK_SUCCESS)
        return fail("vkCreateDescriptorPool", r);
    VkDescriptorSetAllocateInfo dsai = {
        .sType = ST_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = pool,
        .descriptorSetCount = 1,
        .pSetLayouts = &dsl,
    };
    VkDescriptorSet set = 0;
    r = vkAllocateDescriptorSets(dev, &dsai, &set);
    if (r != VK_SUCCESS)
        return fail("vkAllocateDescriptorSets", r);

    VkDescriptorBufferInfo binfo[3] = {
        {bx, 0, VK_WHOLE_SIZE},
        {by, 0, VK_WHOLE_SIZE},
        {bz, 0, VK_WHOLE_SIZE},
    };
    VkWriteDescriptorSet writes[3];
    memset(writes, 0, sizeof(writes));
    for (int i = 0; i < 3; i++) {
        writes[i].sType = ST_WRITE_DESCRIPTOR_SET;
        writes[i].dstSet = set;
        writes[i].dstBinding = (uint32_t)i;
        writes[i].descriptorCount = 1;
        writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        writes[i].pBufferInfo = &binfo[i];
    }
    vkUpdateDescriptorSets(dev, 3, writes, 0, NULL);

    VkCommandPoolCreateInfo cpoci = {
        .sType = ST_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = qfam,
    };
    VkCommandPool cpool = 0;
    r = vkCreateCommandPool(dev, &cpoci, NULL, &cpool);
    if (r != VK_SUCCESS)
        return fail("vkCreateCommandPool", r);
    VkCommandBufferAllocateInfo cbai = {
        .sType = ST_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = cpool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer cmd = NULL;
    r = vkAllocateCommandBuffers(dev, &cbai, &cmd);
    if (r != VK_SUCCESS)
        return fail("vkAllocateCommandBuffers", r);

    VkCommandBufferBeginInfo begin = {.sType = ST_COMMAND_BUFFER_BEGIN_INFO};
    r = vkBeginCommandBuffer(cmd, &begin);
    if (r != VK_SUCCESS)
        return fail("vkBeginCommandBuffer", r);
    vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
    vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set, 0, NULL);
    vkCmdPushConstants(cmd, layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, 4, &a);
    vkCmdDispatch(cmd, n / 32, 1, 1);
    r = vkEndCommandBuffer(cmd);
    if (r != VK_SUCCESS)
        return fail("vkEndCommandBuffer", r);

    VkSubmitInfo submit = {
        .sType = ST_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &cmd,
    };
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    r = vkQueueSubmit(queue, 1, &submit, 0);
    if (r != VK_SUCCESS)
        return fail("vkQueueSubmit", r);
    r = vkQueueWaitIdle(queue);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    if (r != VK_SUCCESS)
        return fail("vkQueueWaitIdle", r);

    uint64_t elapsed_ns = (uint64_t)(t1.tv_sec - t0.tv_sec) * 1000000000ull +
                          (uint64_t)(t1.tv_nsec - t0.tv_nsec);

    uint32_t mismatches = 0;
    float first_got = 0, first_want = 0;
    uint32_t first_i = 0;
    for (uint32_t i = 0; i < n; i++) {
        float want = a * x[i] + y[i];
        if (z[i] != want) {
            if (mismatches == 0) {
                first_i = i;
                first_got = z[i];
                first_want = want;
            }
            mismatches++;
        }
    }

    printf("ecl-vkcompute: device='%s'\n", props.deviceName);
    printf("ecl-vkcompute: n=%u a=%.1f elapsed_ns=%llu\n", n, a, (unsigned long long)elapsed_ns);
    if (mismatches) {
        printf("ecl-vkcompute: FAIL mismatches=%u first i=%u got=%g want=%g\n",
               mismatches, first_i, first_got, first_want);
        return 1;
    }
    printf("ecl-vkcompute: PASS  z[i] = %.1f*x[i] + y[i]  (Vulkan compute / NAK)\n", a);
    return 0;
}

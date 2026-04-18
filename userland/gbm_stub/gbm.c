#include "gbm.h"
#include <stdlib.h>
#include <stdint.h>

struct gbm_device { int fd; };
struct gbm_surface { uint32_t width, height; };
struct gbm_bo { uint32_t width, height; };

struct gbm_device *gbm_create_device(int fd) {
    struct gbm_device *dev = malloc(sizeof(*dev));
    if (dev) dev->fd = fd;
    return dev;
}
void gbm_device_destroy(struct gbm_device *gbm) { free(gbm); }
const char *gbm_device_get_backend_name(struct gbm_device *gbm) { return "stub"; }
struct gbm_surface *gbm_surface_create(struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, uint32_t flags) {
    struct gbm_surface *surf = malloc(sizeof(*surf));
    if (surf) { surf->width = width; surf->height = height; }
    return surf;
}
struct gbm_surface *gbm_surface_create_with_modifiers(struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, const uint64_t *modifiers, const unsigned int count) {
    return gbm_surface_create(gbm, width, height, format, 0);
}
void gbm_surface_destroy(struct gbm_surface *surface) { free(surface); }
struct gbm_bo *gbm_surface_lock_front_buffer(struct gbm_surface *surface) { return NULL; }
void gbm_surface_release_buffer(struct gbm_surface *surface, struct gbm_bo *bo) {}
int gbm_surface_has_free_buffers(struct gbm_surface *surface) { return 0; }
struct gbm_bo *gbm_bo_create(struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, uint32_t flags) { return NULL; }
struct gbm_bo *gbm_bo_create_with_modifiers(struct gbm_device *gbm, uint32_t width, uint32_t height, uint32_t format, const uint64_t *modifiers, const unsigned int count) { return NULL; }
struct gbm_bo *gbm_bo_import(struct gbm_device *gbm, uint32_t type, void *buffer, uint32_t usage) { return NULL; }
void gbm_bo_destroy(struct gbm_bo *bo) {}
uint32_t gbm_bo_get_width(struct gbm_bo *bo) { return 0; }
uint32_t gbm_bo_get_height(struct gbm_bo *bo) { return 0; }
uint32_t gbm_bo_get_stride(struct gbm_bo *bo) { return 0; }
uint32_t gbm_bo_get_format(struct gbm_bo *bo) { return 0; }
struct gbm_device *gbm_bo_get_device(struct gbm_bo *bo) { return NULL; }
union gbm_bo_handle gbm_bo_get_handle(struct gbm_bo *bo) { union gbm_bo_handle h; h.u32 = 0; return h; }
int gbm_bo_get_fd(struct gbm_bo *bo) { return -1; }
int gbm_bo_get_fd_for_plane(struct gbm_bo *bo, int plane)
{
	if (plane != 0)
		return -1;
	return gbm_bo_get_fd(bo);
}
uint32_t gbm_bo_get_offset(struct gbm_bo *bo, int plane) { return 0; }
uint32_t gbm_bo_get_stride_for_plane(struct gbm_bo *bo, int plane) { return 0; }
uint64_t gbm_bo_get_modifier(struct gbm_bo *bo) { return 0; }
int gbm_bo_get_plane_count(struct gbm_bo *bo) { return 0; }
union gbm_bo_handle gbm_bo_get_handle_for_plane(struct gbm_bo *bo, int plane) { union gbm_bo_handle h; h.u32 = 0; return h; }

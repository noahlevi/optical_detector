#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ArgusContext ArgusContext;

/**
 * Create an Argus capture context.
 * Returns NULL on failure.
 */
ArgusContext* argus_create(uint32_t sensor_id, uint32_t width, uint32_t height, uint32_t fps);

/**
 * Acquire one frame into `buffer` (must be >= width*height*3/2 bytes for NV12).
 * `timestamp_ns` receives the Argus sensor monotonic timestamp (CLOCK_MONOTONIC domain).
 * Returns bytes written, or -1 on error/timeout.
 */
int32_t argus_acquire_frame(ArgusContext* ctx,
                            uint8_t* buffer,
                            uint32_t buffer_size,
                            int64_t* timestamp_ns);

/**
 * Stop captures and free all resources.
 */
void argus_destroy(ArgusContext* ctx);

#ifdef __cplusplus
}
#endif

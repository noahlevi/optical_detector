#include "argus_wrapper.h"

#include <Argus/Argus.h>
#include <EGLStream/EGLStream.h>
#include <EGLStream/NV/ImageNativeBuffer.h>
#include <nvbufsurface.h>

#include <cstdio>
#include <cstring>
#include <unistd.h>

using namespace Argus;
using namespace EGLStream;

// ---------------------------------------------------------------------------
// Internal context
// ---------------------------------------------------------------------------

struct ArgusContext {
    UniqueObj<CameraProvider> cameraProvider;
    UniqueObj<CaptureSession> session;
    UniqueObj<OutputStream>   stream;
    UniqueObj<FrameConsumer>  consumer;
    uint32_t width;
    uint32_t height;
};

// ---------------------------------------------------------------------------
// C API implementation
// ---------------------------------------------------------------------------

extern "C" {

ArgusContext* argus_create(uint32_t sensor_id,
                           uint32_t width,
                           uint32_t height,
                           uint32_t fps)
{
    ArgusContext* ctx = new (std::nothrow) ArgusContext();
    if (!ctx) return nullptr;

    ctx->width  = width;
    ctx->height = height;

    // --- CameraProvider ---
    ctx->cameraProvider.reset(CameraProvider::create());
    ICameraProvider* iProvider =
        interface_cast<ICameraProvider>(ctx->cameraProvider);
    if (!iProvider) {
        fprintf(stderr, "[argus] failed to create CameraProvider\n");
        delete ctx;
        return nullptr;
    }

    // --- CameraDevice ---
    std::vector<CameraDevice*> devices;
    if (iProvider->getCameraDevices(&devices) != STATUS_OK ||
        sensor_id >= (uint32_t)devices.size())
    {
        fprintf(stderr, "[argus] sensor_id %u not available (found %zu devices)\n",
                sensor_id, devices.size());
        delete ctx;
        return nullptr;
    }

    // --- CaptureSession ---
    Status status = STATUS_OK;
    ctx->session.reset(
        iProvider->createCaptureSession(devices[sensor_id], &status));
    ICaptureSession* iSession =
        interface_cast<ICaptureSession>(ctx->session);
    if (!iSession || status != STATUS_OK) {
        fprintf(stderr, "[argus] failed to create CaptureSession\n");
        delete ctx;
        return nullptr;
    }

    // --- OutputStream: MAILBOX mode = lowest latency ---
    UniqueObj<OutputStreamSettings> streamSettings(
        iSession->createOutputStreamSettings(STREAM_TYPE_EGL));
    IEGLOutputStreamSettings* iStreamSettings =
        interface_cast<IEGLOutputStreamSettings>(streamSettings);
    if (!iStreamSettings) {
        fprintf(stderr, "[argus] failed to get IEGLOutputStreamSettings\n");
        delete ctx;
        return nullptr;
    }
    iStreamSettings->setPixelFormat(PIXEL_FMT_YCbCr_420_888);
    iStreamSettings->setResolution(Size2D<uint32_t>(width, height));
    iStreamSettings->setMetadataEnable(true);
    // MAILBOX: producer overwrites the buffer if consumer hasn't read yet,
    // instead of queuing — this eliminates pipeline buildup.
    iStreamSettings->setMode(EGL_STREAM_MODE_MAILBOX);

    ctx->stream.reset(iSession->createOutputStream(streamSettings.get()));
    if (!ctx->stream) {
        fprintf(stderr, "[argus] failed to create OutputStream\n");
        delete ctx;
        return nullptr;
    }

    // --- FrameConsumer ---
    ctx->consumer.reset(FrameConsumer::create(ctx->stream.get()));
    IFrameConsumer* iConsumer =
        interface_cast<IFrameConsumer>(ctx->consumer);
    if (!iConsumer) {
        fprintf(stderr, "[argus] failed to create FrameConsumer\n");
        delete ctx;
        return nullptr;
    }

    // --- Request ---
    UniqueObj<Request> request(iSession->createRequest(CAPTURE_INTENT_PREVIEW));
    IRequest* iRequest = interface_cast<IRequest>(request);
    if (!iRequest) {
        fprintf(stderr, "[argus] failed to create Request\n");
        delete ctx;
        return nullptr;
    }
    iRequest->enableOutputStream(ctx->stream.get());

    // Frame rate + exposure
    ISourceSettings* iSource =
        interface_cast<ISourceSettings>(iRequest->getSourceSettings());
    if (iSource) {
        uint64_t dur_ns = 1000000000ULL / fps;
        iSource->setFrameDurationRange(Range<uint64_t>(dur_ns, dur_ns));

        // Exposure: let AE run freely by default.
        // Override with CAM_EXPOSURE_US to pin a fixed exposure (disables AE).
        const char* exp_env = getenv("CAM_EXPOSURE_US");
        if (exp_env) {
            uint64_t exposure_ns = (uint64_t)atol(exp_env) * 1000ULL;
            iSource->setExposureTimeRange(Range<uint64_t>(exposure_ns, exposure_ns));
            fprintf(stderr, "[argus] fixed exposure=%.1fms (AE disabled)\n", exposure_ns / 1e6);
        } else {
            fprintf(stderr, "[argus] AE enabled (auto exposure)\n");
        }
    }

    // --- Start repeating captures ---
    if (iSession->repeat(request.get()) != STATUS_OK) {
        fprintf(stderr, "[argus] failed to start repeat capture\n");
        delete ctx;
        return nullptr;
    }

    fprintf(stderr, "[argus] started %ux%u @ %u fps (MAILBOX mode)\n",
            width, height, fps);
    return ctx;
}

int32_t argus_acquire_frame(ArgusContext* ctx,
                             uint8_t*     buffer,
                             uint32_t     buffer_size,
                             int64_t*     timestamp_ns)
{
    IFrameConsumer* iConsumer =
        interface_cast<IFrameConsumer>(ctx->consumer);

    // Wait up to 1 second for a frame
    UniqueObj<Frame> frame(iConsumer->acquireFrame(1000000000ULL));
    if (!frame) {
        fprintf(stderr, "[argus] acquireFrame timed out\n");
        return -1;
    }

    IFrame* iFrame = interface_cast<IFrame>(frame);
    if (!iFrame) return -1;

    // Record monotonic time at frame delivery (CLOCK_MONOTONIC, nanoseconds)
    if (timestamp_ns) {
        struct timespec tp;
        clock_gettime(CLOCK_MONOTONIC, &tp);
        *timestamp_ns = (int64_t)tp.tv_sec * 1000000000LL + tp.tv_nsec;
    }

    // --- Map frame pixels via NvBufSurface ---
    NV::IImageNativeBuffer* iNative =
        interface_cast<NV::IImageNativeBuffer>(iFrame->getImage());
    if (!iNative) {
        fprintf(stderr, "[argus] IImageNativeBuffer not available\n");
        return -1;
    }

    // createNvBuffer returns a dmabuf fd for NV12 data
    int fd = iNative->createNvBuffer(
        Size2D<uint32_t>(ctx->width, ctx->height),
        NVBUF_COLOR_FORMAT_NV12,
        NVBUF_LAYOUT_PITCH);
    if (fd < 0) {
        fprintf(stderr, "[argus] createNvBuffer failed\n");
        return -1;
    }

    NvBufSurface* surf = nullptr;
    if (NvBufSurfaceFromFd(fd, (void**)&surf) < 0) {
        fprintf(stderr, "[argus] NvBufSurfaceFromFd failed\n");
        close(fd);
        return -1;
    }

    if (NvBufSurfaceMap(surf, 0, -1, NVBUF_MAP_READ) < 0) {
        fprintf(stderr, "[argus] NvBufSurfaceMap failed\n");
        NvBufSurfaceDestroy(surf);
        close(fd);
        return -1;
    }

    NvBufSurfaceSyncForCpu(surf, 0, -1);

    uint32_t y_size  = ctx->width * ctx->height;
    uint32_t uv_size = y_size / 2;
    uint32_t total   = y_size + uv_size;

    // Read actual strides from surface (may differ from width due to alignment)
    uint32_t y_stride  = surf->surfaceList[0].planeParams.pitch[0];
    uint32_t uv_stride = surf->surfaceList[0].planeParams.pitch[1];
    static bool stride_logged = false;
    if (!stride_logged) {
        fprintf(stderr, "[argus] y_stride=%u uv_stride=%u\n", y_stride, uv_stride);
        stride_logged = true;
    }

    int32_t written = -1;
    if (buffer_size >= total) {
        uint8_t* y_addr  = (uint8_t*)surf->surfaceList[0].mappedAddr.addr[0];
        uint8_t* uv_addr = (uint8_t*)surf->surfaceList[0].mappedAddr.addr[1];
        if (y_addr && uv_addr) {
            // Copy Y plane row by row to strip padding
            for (uint32_t row = 0; row < ctx->height; row++)
                memcpy(buffer + row * ctx->width, y_addr + row * y_stride, ctx->width);
            // Copy UV plane row by row (height/2 rows)
            for (uint32_t row = 0; row < ctx->height / 2; row++)
                memcpy(buffer + y_size + row * ctx->width, uv_addr + row * uv_stride, ctx->width);
            written = (int32_t)total;
        }
    }

    NvBufSurfaceUnMap(surf, 0, -1);
    NvBufSurfaceDestroy(surf);
    close(fd);

    return written;
}

void argus_destroy(ArgusContext* ctx)
{
    if (!ctx) return;
    ICaptureSession* iSession =
        interface_cast<ICaptureSession>(ctx->session);
    if (iSession) {
        iSession->stopRepeat();
        iSession->waitForIdle();
    }
    delete ctx;
    fprintf(stderr, "[argus] context destroyed\n");
}

} // extern "C"

// The PyroWave Metal port decodes into textures only. This decodes into
// three shared-storage textures and copies the planes out, giving the Rust
// side the same CPU interface as the Vulkan library's
// pyrowave_decoder_decode_cpu_buffer_synchronous.

#import <Metal/Metal.h>
#include "pyrowave_metal.h"

namespace
{
struct Readback
{
	id<MTLDevice> device;
	id<MTLCommandQueue> queue;
	id<MTLTexture> planes[3];
	int width;
	int height;
};
}

extern "C" void *silkscreen_pyrowave_readback_new(int width, int height)
{
	@autoreleasepool
	{
		// The same device pyrowave_create_default_device picks.
		id<MTLDevice> device = MTLCreateSystemDefaultDevice();
		if (!device)
			return nullptr;
		auto *r = new Readback();
		r->device = device;
		r->queue = [device newCommandQueue];
		r->width = width;
		r->height = height;
		for (int p = 0; p < 3; p++)
		{
			int w = p ? width / 2 : width;
			int h = p ? height / 2 : height;
			MTLTextureDescriptor *d =
			    [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatR8Unorm
			                                                       width:w
			                                                      height:h
			                                                   mipmapped:NO];
			d.usage = MTLTextureUsageShaderWrite | MTLTextureUsageShaderRead;
			d.storageMode = MTLStorageModeShared;
			r->planes[p] = [device newTextureWithDescriptor:d];
			if (!r->planes[p] || !r->queue)
			{
				delete r;
				return nullptr;
			}
		}
		return r;
	}
}

extern "C" pyrowave_result silkscreen_pyrowave_readback_decode(void *readback, pyrowave_decoder decoder,
                                                               const pyrowave_cpu_buffer *out)
{
	@autoreleasepool
	{
		auto *r = static_cast<Readback *>(readback);
		if (!r || !out || out->width != r->width || out->height != r->height)
			return PYROWAVE_ERROR_INVALID_ARGUMENT;
		id<MTLCommandBuffer> cb = [r->queue commandBuffer];
		pyrowave_gpu_buffers buffers = {
			{ (__bridge void *)r->planes[0], (__bridge void *)r->planes[1], (__bridge void *)r->planes[2] }
		};
		pyrowave_result res = pyrowave_decoder_decode_gpu_buffer(decoder, (__bridge void *)cb, &buffers);
		if (res != PYROWAVE_SUCCESS)
			return res;
		[cb commit];
		[cb waitUntilCompleted];
		if (cb.status != MTLCommandBufferStatusCompleted)
			return PYROWAVE_ERROR_GENERIC;
		for (int p = 0; p < 3; p++)
		{
			int w = p ? r->width / 2 : r->width;
			int h = p ? r->height / 2 : r->height;
			[r->planes[p] getBytes:out->data[p]
			           bytesPerRow:out->row_stride_in_bytes[p]
			            fromRegion:MTLRegionMake2D(0, 0, w, h)
			           mipmapLevel:0];
		}
		return PYROWAVE_SUCCESS;
	}
}

extern "C" void silkscreen_pyrowave_readback_free(void *readback)
{
	delete static_cast<Readback *>(readback);
}

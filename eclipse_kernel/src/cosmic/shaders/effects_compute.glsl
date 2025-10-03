#version 450 core

layout(local_size_x = 16, local_size_y = 16, local_size_z = 1) in;

layout(rgba8, binding = 0) uniform image2D inputImage;
layout(rgba8, binding = 1) uniform image2D outputImage;

uniform float uTime;
uniform float uEffectStrength;
uniform int uEffectType;

void main()
{
    ivec2 pixel = ivec2(gl_GlobalInvocationID.xy);
    ivec2 imageSize = imageSize(inputImage);
    
    if (pixel.x >= imageSize.x || pixel.y >= imageSize.y) {
        return;
    }
    
    vec4 color = imageLoad(inputImage, pixel);
    
    // Apply different effects based on uEffectType
    if (uEffectType == 0) {
        // Gaussian blur
        vec4 blurred = vec4(0.0);
        float weight = 0.0;
        
        for (int x = -2; x <= 2; x++) {
            for (int y = -2; y <= 2; y++) {
                ivec2 offset = pixel + ivec2(x, y);
                if (offset.x >= 0 && offset.x < imageSize.x && 
                    offset.y >= 0 && offset.y < imageSize.y) {
                    float gauss = exp(-(x*x + y*y) / (2.0 * 1.5 * 1.5));
                    blurred += imageLoad(inputImage, offset) * gauss;
                    weight += gauss;
                }
            }
        }
        color = blurred / weight;
    }
    else if (uEffectType == 1) {
        // Glow effect
        float intensity = dot(color.rgb, vec3(0.299, 0.587, 0.114));
        color.rgb += color.rgb * intensity * uEffectStrength;
    }
    else if (uEffectType == 2) {
        // Noise effect
        float noise = fract(sin(dot(vec2(pixel), vec2(12.9898, 78.233))) * 43758.5453);
        color.rgb += (noise - 0.5) * uEffectStrength;
    }
    
    imageStore(outputImage, pixel, color);
}

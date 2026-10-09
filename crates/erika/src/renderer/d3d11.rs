use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{HANDLE, HMODULE, HWND};
use ::windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use ::windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY, ID3DBlob,
};
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE,
    D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BOX, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_RESOURCE_MISC_SHARED, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_ARRAY_SRV, D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE3D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, D3D11CreateDevice, ID3D11BlendState,
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout, ID3D11Multithread,
    ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_R8_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R32G32_FLOAT, DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_FRAME_STATISTICS, DXGI_HDR_METADATA_HDR10,
    DXGI_HDR_METADATA_TYPE_HDR10, DXGI_HDR_METADATA_TYPE_NONE, DXGI_PRESENT_DO_NOT_WAIT,
    DXGI_PRESENT_PARAMETERS, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIResource,
    IDXGISwapChain, IDXGISwapChain1, IDXGISwapChain3, IDXGISwapChain4,
};
use ::windows::core::{BOOL, IUnknown, Interface, PCSTR};

use crate::core::{
    ColorPrimaries, FlutterTextureKind, LumaUpscalerBackendStatus, PlatformSurface, PlayerError,
    PlayerVideoFrame, RenderFrameContext, RendererBackend, RendererRuntimeStats, Result,
    SurfaceMetrics, TransferFunction, WgpuSurfaceKind,
};
use crate::danmaku::{
    DanmakuAtlasUpdate, DanmakuGlyphAtlas, DanmakuGlyphInstance, DanmakuRenderPlan,
};
use crate::ffmpeg::{Frame, PlanarFrame, PlanarPixelFormat};
use crate::overlay::OverlayFrame;
use crate::renderer::d3d11_artcnn::D3d11ArtCnn;
use crate::renderer::gamut::{
    GamutLutJob, GamutLutParams, LUT_SIZE_C, LUT_SIZE_H, LUT_SIZE_I, pack_rgba16f,
};
use crate::renderer::metal::{MetalRendererConfig, VideoAlphaMode};
use crate::renderer::output::{
    ActiveOutputEncoding, OutputFallbackReason, OutputRuntimeStatus, OutputSurfaceFormat,
};
use crate::renderer::pipeline::{
    Chromaticity, LumaUpscalerMode, PrimariesCoordinates, SourceColorState, TargetColorState,
    VideoRenderPipeline, VideoUniforms, primaries_coordinates,
};
use crate::renderer::presentation::PresentationLayout;
use crate::subtitle::AssColor;

const SDR_SWAPCHAIN_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
const HDR10_SWAPCHAIN_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R10G10B10A2_UNORM;
const SHADER_SOURCE: &[u8] = br#"
struct VsIn {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    float2 texcoord : TEXCOORD0;
};

struct DoviUniforms {
    float4 dovi_flags;
    float4 dovi_pivots[6];
    float4 dovi_bounds[3];
    float4 dovi_coefficients[24];
    float4 dovi_mmr[144];
    float4 dovi_nonlinear_matrix[3];
    float4 dovi_nonlinear_offset;
    float4 dovi_lms_matrix[3];
};

cbuffer VideoConstants : register(b0) {
    uint is_p010;
    uint full_range;
    uint source_transfer;
    uint target_transfer;
    uint tone_map;
    uint edr_output;
    uint input_mode;
    uint scene_linear;
    float4 nits;
    float4 luma_coefficients;
    float4 gamut_matrix_rows[3];
    float4 ipt_matrix_rows[9];
    float4 tone_map_extra;
    float4 tone_map_coeffs;
    uint gamut_lut_enabled;
    uint gamut_primaries;
    uint gamut_reserved0;
    uint gamut_reserved1;
    DoviUniforms dovi;
    // xy scales native/packed luma coordinates; zw scales native chroma.
    // D3D11VA textures can be allocation-aligned beyond the visible frame.
    float4 texture_scales;
};

Texture2D lumaTex : register(t0);
Texture2D chromaTex : register(t1);
Texture3D gamutLut : register(t2);
SamplerState videoSampler : register(s0);

float source_peak_nits() {
    return max(nits.x, 1.0);
}

float target_peak_nits() {
    return max(nits.y, 1.0);
}

float source_reference_white_nits() {
    return max(nits.z, 1.0);
}

float target_reference_white_nits() {
    return max(nits.w, 1.0);
}

float pq_eotf(float encoded) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(max(encoded, 0.0), 1.0 / m2);
    float num = max(p - c1, 0.0);
    float den = max(c2 - c3 * p, 0.000001);
    return pow(num / den, 1.0 / m1);
}

float pq_inverse_eotf(float normalized_nits) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(clamp(normalized_nits, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / max(1.0 + c3 * p, 0.000001), m2);
}

// BT.2100 HLG inverse OETF: nonlinear signal E' to scene linear light in
// [0, 1]. Mirrors the Rust reference implementation in
// `renderer/pipeline.rs` tests (`hlg_inverse_oetf`).
float hlg_inverse_oetf(float encoded) {
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    float e = max(encoded, 0.0);
    if (e <= 0.5) {
        return e * e / 3.0;
    }
    return (exp((e - c) / a) + b) / 12.0;
}

float3 transfer_to_source_reference_linear(float3 rgb_in) {
    float3 rgb = max(rgb_in, float3(0.0, 0.0, 0.0));
    if (source_transfer == 3u) {
        const float pq_absolute_peak_nits = 10000.0;
        return float3(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b))
            * (pq_absolute_peak_nits / source_reference_white_nits());
    }
    if (source_transfer == 4u) {
        // HLG: inverse OETF to scene linear, then the BT.2100 OOTF (system
        // gamma 1.2 at the 1000 nit nominal peak) to display linear,
        // normalized to source reference white like the PQ branch above.
        const float hlg_nominal_peak_nits = 1000.0;
        const float hlg_system_gamma = 1.2;
        float3 scene = float3(
            hlg_inverse_oetf(rgb.r),
            hlg_inverse_oetf(rgb.g),
            hlg_inverse_oetf(rgb.b)
        );
        float scene_luma = max(dot(luma_coefficients.xyz, scene), 0.000001);
        return scene * (hlg_nominal_peak_nits * pow(scene_luma, hlg_system_gamma - 1.0)
            / source_reference_white_nits());
    }
    if (source_transfer == 1u) {
        return pow(rgb, float3(2.2, 2.2, 2.2));
    }
    if (source_transfer == 2u) {
        return pow(rgb, float3(2.4, 2.4, 2.4));
    }
    return rgb;
}

float3 source_reference_to_nits(float3 rgb) {
    return max(rgb, float3(0.0, 0.0, 0.0)) * source_reference_white_nits();
}

float pq_code(float nits) {
    return pq_inverse_eotf(clamp(nits, 0.0, 10000.0) / 10000.0);
}

float nits_from_pq(float code) {
    return 10000.0 * pq_eotf(clamp(code, 0.0, 1.0));
}

// libplacebo pl_smoothstep with arbitrary edge order (HLSL smoothstep has
// undefined results when edge0 >= edge1, and libplacebo's knee tuning term
// deliberately uses reversed edges).
float sstep(float edge0, float edge1, float x) {
    float t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Simple primaries conversion (HDR10 output path); the tone-mapped path
// converts primaries inside the IPT roundtrip instead.
float3 apply_gamut_map(float3 rgb) {
    return float3(
        dot(gamut_matrix_rows[0].xyz, rgb),
        dot(gamut_matrix_rows[1].xyz, rgb),
        dot(gamut_matrix_rows[2].xyz, rgb)
    );
}

// libplacebo st2094_pick_knee evaluated on absolute PQ codes. The source
// pivot follows the scene average luminance when known and stays within
// [10%, 80%] of the range; the destination pivot rescales it into the output
// range and then adapts towards the 1:1 line (knee_adaptation 0.4).
float2 st2094_pick_knee(float src_min, float src_max, float src_avg, float dst_min, float dst_max) {
    const float knee_adaptation = 0.4;
    const float min_knee = 0.1;
    const float max_knee = 0.8;
    const float def_knee = 0.4;
    float src_knee_min = lerp(src_min, src_max, min_knee);
    float src_knee_max = lerp(src_min, src_max, max_knee);
    float dst_knee_min = lerp(dst_min, dst_max, min_knee);
    float dst_knee_max = lerp(dst_min, dst_max, max_knee);
    float fallback = lerp(src_min, src_max, def_knee);
    float src_knee = clamp(src_avg > 0.0 ? src_avg : fallback, src_knee_min, src_knee_max);
    float target = (src_knee - src_min) / max(src_max - src_min, 0.000001);
    float adapted = lerp(dst_min, dst_max, target);
    float tuning = 1.0 - sstep(max_knee, def_knee, target) * sstep(min_knee, def_knee, target);
    float adaptation = lerp(knee_adaptation, 1.0, tuning);
    float dst_knee = clamp(lerp(src_knee, adapted, adaptation), dst_knee_min, dst_knee_max);
    return float2(src_knee, dst_knee);
}

// The tone-map curve evaluated on the IPT intensity axis (PQ codes),
// mirroring libplacebo's tone-map functions. `param` is the per-operator
// curve parameter from ToneMapConfig::curve_param (0 = operator default).
float tone_map_curve_pq(float x_in, float param) {
    float src_peak = source_peak_nits();
    float dst_peak = target_peak_nits();
    float src_avg = tone_map_extra.y;
    float dst_black = tone_map_extra.z;
    float in_min = 0.0;
    float in_max = max(pq_code(src_peak), 0.000001);
    float out_min = pq_code(dst_black);
    float out_max = max(pq_code(dst_peak), 0.000001);
    float out_range = max(out_max - out_min, 0.000001);
    float x = clamp(x_in, in_min, in_max);
    if (tone_map == 0u) {
        // Clip: values within the source range pass through untouched.
        return x;
    }
    if (tone_map == 1u) {
        // Reinhard (output-relative, libplacebo pl_tone_map_reinhard).
        //
        // libplacebo evaluates this curve in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators (BT.2390, spline, ST 2094-10)
        // work on encoded values. Running it on PQ codes bends the curve's
        // meaning: against a 1000->203 nit target mid-tones darken badly, and
        // the Mobius knee lands far below its intended position. Rescale to
        // linear nits here and encode back at the end.
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float contrast = param > 0.0 ? param : 0.5;
        float offset = (1.0 - contrast) / max(contrast, 0.000001);
        float scale = (peak + offset) / peak;
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t / (t + offset) * scale;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (tone_map == 2u) {
        // Mobius: Mobius transform with a 1:1 linear region below the knee,
        // also evaluated in linear nits (see the Reinhard note above). The
        // knee j is relative to the output range, so the linear region ends
        // at dst_black + j * (dst_peak - dst_black).
        float in_max_nits = max(src_peak, 0.000001);
        float out_min_nits = dst_black;
        float out_range_nits = max(dst_peak - dst_black, 0.000001);
        float peak = in_max_nits / out_range_nits;
        float j = param > 0.0 ? param : 0.3;
        float a = -j * j * (peak - 1.0) / (j * j - 2.0 * j + peak);
        float b = (j * j - 2.0 * j * peak + peak) / max(peak - 1.0, 0.000001);
        float scale = (b * b + 2.0 * b * j + j * j) / (b - a);
        float t = clamp(nits_from_pq(x), 0.0, in_max_nits) / out_range_nits;
        float mapped = t > j ? scale * (t + a) / (t + b) : t;
        return pq_code(mapped * out_range_nits + out_min_nits);
    }
    if (tone_map == 3u) {
        // ITU-R BT.2390 EETF with black-point compensation (the libplacebo
        // version also compensates target black; the earlier port skipped it).
        float knee_offset = param > 0.0 ? param : 1.0;
        float max_lum = clamp(out_max / in_max, 0.0, 1.0);
        float min_lum = out_min / in_max;
        float ks = (1.0 + knee_offset) * max_lum - knee_offset;
        float bp = min(max(1.0 / max(min_lum, 0.000001), 0.0), 4.0);
        float u = x / in_max;
        if (ks < 1.0 && u > ks) {
            float tb = (u - ks) / (1.0 - ks);
            float tb2 = tb * tb;
            float tb3 = tb2 * tb;
            u = (2.0 * tb3 - 3.0 * tb2 + 1.0) * ks
              + (tb3 - 2.0 * tb2 + tb) * (1.0 - ks)
              + (-2.0 * tb3 + 3.0 * tb2) * max_lum;
        }
        if (u < 1.0) {
            u = u + min_lum * pow(1.0 - u, bp);
            float gain = max_lum < 1.0
                ? 1.0 / (1.0 + min_lum / max_lum * pow(1.0 - max_lum, bp))
                : 1.0;
            u = gain * (u - min_lum) + min_lum;
        }
        return u * in_max;
    }
    if (tone_map == 4u) {
        // Spline: perceptually linear single-pivot polynomial, the default
        // tone map of libplacebo and mpv's gpu-next renderer.
        float contrast = param > 0.0 ? param : 0.3;
        float fallback_avg = clamp(0.4 * src_peak, 100.0, 400.0);
        float effective_src_avg = src_avg > 0.0 ? src_avg : fallback_avg;
        float2 knee = st2094_pick_knee(
            in_min,
            in_max,
            pq_code(effective_src_avg),
            out_min,
            out_max
        );
        float src_pivot = knee.x;
        float dst_pivot = knee.y;
        float slope0 = (dst_pivot - out_min) / max(src_pivot - in_min, 0.000001);
        float ratio = clamp(1.5 * (in_max / out_max - 1.0), 0.2, 1.2);
        float slope = pow(slope0, (1.0 - contrast) * ratio);
        float in_min0 = in_min - src_pivot;
        float in_max0 = in_max - src_pivot;
        float out_min0 = out_min - dst_pivot;
        float out_max0 = out_max - dst_pivot;
        float pa = (out_min0 - slope * in_min0) / (in_min0 * in_min0);
        float qa = (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0 * in_max0);
        float qb = -3.0 * (slope * in_max0 - out_max0) / (2.0 * in_max0 * in_max0);
        float xr = x - src_pivot;
        float mapped = xr > 0.0
            ? ((qa * xr + qb) * xr + slope) * xr
            : (pa * xr + slope) * xr;
        return mapped + dst_pivot;
    }
    if (tone_map == 5u) {
        // ITU-R BT.2446 method A: Weber-law log compression from the source
        // peak envelope and a standardized S-curve (mpv's recommended curve
        // for well-mastered content).
        float phdr = 1.0 + 32.0 * pow(src_peak / 10000.0, 1.0 / 2.4);
        float psdr = 1.0 + 32.0 * pow(dst_peak / 10000.0, 1.0 / 2.4);
        float t = pow(nits_from_pq(x) / max(src_peak, 0.000001), 1.0 / 2.4);
        t = log(1.0 + (phdr - 1.0) * t) / log(phdr);
        if (t <= 0.7399) {
            t = 1.0770 * t;
        } else if (t < 0.9909) {
            t = (-1.1510 * t + 2.7811) * t - 0.6302;
        } else {
            t = 0.5 * t + 0.5;
        }
        t = (pow(psdr, t) - 1.0) / (psdr - 1.0);
        // BT.1886 EOTF from the target black point and peak.
        float lb = pow(max(dst_black, 0.0), 1.0 / 2.4);
        float lw = pow(max(dst_peak, 0.0), 1.0 / 2.4);
        return pq_code(pow((lw - lb) * t + lb, 2.4));
    }
    // SMPTE ST 2094-10 (DolbyVision's dynamic-metadata curve): rational
    // Mobius interpolation in absolute nits; coefficients are solved per
    // frame on the CPU from the same scene pivot.
    float c1 = tone_map_coeffs.x;
    float c2 = tone_map_coeffs.y;
    float c3 = tone_map_coeffs.z;
    float x_nits = nits_from_pq(x);
    float y_nits = (c1 + c2 * x_nits) / max(1.0 + c3 * x_nits, 0.000001);
    return pq_code(clamp(y_nits, 0.0, 10000.0));
}

float3 tone_map_nits(float3 input_nits) {
    if (target_transfer == 3u) {
        // HDR10 output: convert primaries by the gamut matrix and clamp to
        // the PQ range (no tone mapping; the display does the HDR mapping).
        return clamp(apply_gamut_map(max(input_nits, float3(0.0, 0.0, 0.0)) / source_reference_white_nits())
            * source_reference_white_nits(), float3(0.0, 0.0, 0.0), float3(10000.0, 10000.0, 10000.0));
    }
    // libplacebo color map: RGB in source primaries (absolute nits) to
    // HPE-LMS, PQ-encode, IPT, map the intensity axis and apply the
    // hue-preserving chroma rule, then decode back to RGB in the target
    // primaries. The primaries conversion happens inside this roundtrip.
    float3 rgb = max(input_nits, float3(0.0, 0.0, 0.0));
    float3 lms = float3(
        dot(ipt_matrix_rows[0].xyz, rgb),
        dot(ipt_matrix_rows[1].xyz, rgb),
        dot(ipt_matrix_rows[2].xyz, rgb)
    );
    float3 lmspq = float3(pq_code(lms.r), pq_code(lms.g), pq_code(lms.b));
    float3 ipt = float3(
        dot(float3(0.4, 0.4, 0.2), lmspq),
        dot(float3(4.455, -4.851, 0.396), lmspq),
        dot(float3(0.8056, 0.3572, -1.1628), lmspq)
    );
    float i_orig = ipt.x;
    ipt.x = tone_map_curve_pq(ipt.x, tone_map_extra.x);
    // Libplacebo's chroma rule: clamp the saturation boost when brightening
    // and desaturate (by the cubic hull term) when the mapping darkens.
    float2 hull = float2(i_orig, ipt.x);
    float2 hull_c = ((hull - float2(6.0, 6.0)) * hull + float2(9.0, 9.0)) * hull;
    float ratio = min(i_orig / max(ipt.x, 0.000001), hull_c.y / max(hull_c.x, 0.000001));
    ipt.yz = ipt.yz * ratio;
    if (gamut_lut_enabled != 0u) {
        // I axis spans the target's [black, peak] in PQ codes, matching
        // libplacebo's gamut.min_luma/max_luma (tone_map_extra.z is the
        // target black in nits, the same value the LUT was generated for).
        float lut_min = pq_code(tone_map_extra.z);
        float lut_max = max(pq_code(target_peak_nits()), 0.000001);
        float lut_range = max(lut_max - lut_min, 0.000001);
        float3 pos = float3(
            clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0),
            clamp(2.0 * length(ipt.yz), 0.0, 1.0),
            0.5 + 0.5 * atan2(ipt.z, ipt.y) / 3.14159265
        );
        // libplacebo's texel_scale: the lattice position must be remapped to
        // the texel-center coordinate, otherwise the low end of the chroma
        // axis (whose first texel stores zero chroma) leaks in and crushes
        // saturation.
        float3 idx = float3(
            pos.x * (47.0 / 48.0) + 0.5 / 48.0,
            pos.y * (31.0 / 32.0) + 0.5 / 32.0,
            pos.z * (255.0 / 256.0) + 0.5 / 256.0
        );
        float3 sampled = gamutLut.Sample(videoSampler, idx).xyz;
        ipt = float3(sampled.x, sampled.y - 0.5, sampled.z - 0.5);
    }
    float3 lmspq_out = float3(
        dot(float3(1.0, 0.0975689, 0.205226), ipt),
        dot(float3(1.0, -0.113876, 0.133217), ipt),
        dot(float3(1.0, 0.0326151, -0.676887), ipt)
    );
    float3 lms_out = float3(
        nits_from_pq(lmspq_out.r),
        nits_from_pq(lmspq_out.g),
        nits_from_pq(lmspq_out.b)
    );
    return float3(
        dot(ipt_matrix_rows[6].xyz, lms_out),
        dot(ipt_matrix_rows[7].xyz, lms_out),
        dot(ipt_matrix_rows[8].xyz, lms_out)
    );
}

// Hue-preserving gamut mapping: the linear gamut matrix can push highly
// saturated wide-gamut colors outside the target gamut (negative
// components). Blending those towards luma shifts hue - BT.2020 primary
// red picks up blue and turns pink. Instead blend towards the naive clip
// by an out-of-gamut smoothstep factor: slightly-out colors stay nearly
// intact, strongly-out primaries land on the pure target primary with
// their hue intact, matching mpv's perceptual gamut handling. Mirrors the
// WGSL/MSL gamut_compress and the Rust reference in pipeline.rs tests.
// Brightness overshoot (> 1) is left for the tone map.
float3 gamut_compress(float3 rgb) {
    float lo = min(rgb.r, min(rgb.g, rgb.b));
    float outness = max(-lo, 0.0);
    float k = smoothstep(0.0, 1.0, outness);
    return lerp(rgb, clamp(rgb, 0.0, 1.0), k);
}

float3 target_nits_to_reference_linear(float3 input_nits) {
    // libplacebo's encode maps [target black, target peak] onto [0, 1] where
    // 1.0 is the target reference white, so the tone-map black-point
    // compensation lands back on true black instead of lifting it.
    float black = tone_map_extra.z;
    float peak = target_peak_nits();
    float range = max(peak - black, 0.0001);
    return max(input_nits - float3(black, black, black), float3(0.0, 0.0, 0.0)) / range
        * (range / target_reference_white_nits());
}

float3 target_reference_linear_to_output(float3 rgb) {
    if (scene_linear != 0u) {
        return max(rgb, float3(0.0, 0.0, 0.0));
    }
    if (target_transfer == 3u) {
        const float pq_absolute_peak_nits = 10000.0;
        float3 out_nits = max(rgb, float3(0.0, 0.0, 0.0)) * target_reference_white_nits();
        return float3(
            pq_inverse_eotf(out_nits.r / pq_absolute_peak_nits),
            pq_inverse_eotf(out_nits.g / pq_absolute_peak_nits),
            pq_inverse_eotf(out_nits.b / pq_absolute_peak_nits)
        );
    }
    if (edr_output != 0u) {
        return max(rgb, float3(0.0, 0.0, 0.0));
    }
    if (target_transfer == 1u) {
        return pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(1.0 / 2.2, 1.0 / 2.2, 1.0 / 2.2));
    }
    if (target_transfer == 2u) {
        return pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(1.0 / 2.4, 1.0 / 2.4, 1.0 / 2.4));
    }
    return rgb;
}

float4 final_output(float3 rgb, float alpha) {
    float3 output_rgb;
    if (scene_linear != 0u) {
        output_rgb = max(rgb, float3(0.0, 0.0, 0.0)) * alpha;
        return float4(output_rgb, alpha);
    }
    if (target_transfer == 3u) {
        output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(1.0, 1.0, 1.0)) * alpha;
        return float4(output_rgb, alpha);
    }
    if (edr_output != 0u) {
        float headroom = max(target_peak_nits() / target_reference_white_nits(), 1.0);
        output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(headroom, headroom, headroom)) * alpha;
        return float4(output_rgb, alpha);
    }
    output_rgb = clamp(rgb, float3(0.0, 0.0, 0.0), float3(1.0, 1.0, 1.0)) * alpha;
    return float4(output_rgb, alpha);
}

void expand_ycbcr_range(float y_in, float2 cbcr_in, out float y, out float2 cbcr) {
    if (is_p010 != 0u) {
        // P010 stores 10-bit codes as code << 6 in a 16-bit UNORM texture.
        const float p010_scale = 65535.0 / 65472.0;
        y_in *= p010_scale;
        cbcr_in *= p010_scale;
    }
    if (full_range != 0u) {
        y = y_in;
        cbcr = cbcr_in - float2(0.5, 0.5);
        return;
    }
    if (is_p010 != 0u) {
        y = (y_in - (64.0 / 1023.0)) * (1023.0 / 876.0);
        cbcr = (cbcr_in - float2(512.0 / 1023.0, 512.0 / 1023.0)) * (1023.0 / 896.0);
        return;
    }
    y = (y_in - (16.0 / 255.0)) * (255.0 / 219.0);
    cbcr = (cbcr_in - float2(128.0 / 255.0, 128.0 / 255.0)) * (255.0 / 224.0);
}

// Dolby Vision RPU reshaping, ported from libplacebo's `pl_shader_dovi_reshape`
// (the renderer behind mpv's Dolby Vision mapping). The base-layer signal is
// reshaped per component through piecewise polynomial/MMR curves selected by
// pivot comparison, where MMR coefficients mix all three raw components.
float3 dovi_reshaped_signal(float3 sig_in) {
    float3 sig = clamp(sig_in, 0.0, 1.0);
    float result[3] = { sig.r, sig.g, sig.b };
    float4 flags = dovi.dovi_flags;
    for (uint c = 0; c < 3u; c++) {
        uint segments = uint(flags[1u + c]);
        if (segments == 0u) {
            continue;
        }
        float s = result[c];
        uint index = 0u;
        for (uint i = 0u; i < 7u; i++) {
            float4 pivot_row = dovi.dovi_pivots[2u * c + i / 4u];
            float pivot = pivot_row[i % 4u];
            if (s >= pivot) {
                index = index + 1u;
            }
        }
        float4 coeff = dovi.dovi_coefficients[8u * c + index];
        if (coeff.w < 0.5) {
            s = (coeff.z * s + coeff.y) * s + coeff.x;
        } else {
            uint base = 48u * c + uint(coeff.y);
            uint order = uint(coeff.w);
            float4 sig_x = float4(
                sig.x * sig.y,
                sig.x * sig.z,
                sig.y * sig.z,
                sig.x * sig.y * sig.z
            );
            s = coeff.x;
            s = s + dot(dovi.dovi_mmr[base].xyz, sig);
            s = s + dot(dovi.dovi_mmr[base + 1u], sig_x);
            if (order >= 2u) {
                float3 sig2 = sig * sig;
                float4 sig_x2 = sig_x * sig_x;
                s = s + dot(dovi.dovi_mmr[base + 2u].xyz, sig2);
                s = s + dot(dovi.dovi_mmr[base + 3u], sig_x2);
                if (order >= 3u) {
                    s = s + dot(dovi.dovi_mmr[base + 4u].xyz, sig2 * sig);
                    s = s + dot(dovi.dovi_mmr[base + 5u], sig_x2 * sig_x);
                }
            }
        }
        float4 bounds = dovi.dovi_bounds[c];
        result[c] = clamp(s, bounds.x, bounds.y);
    }
    return float3(result[0], result[1], result[2]);
}

// Reshaped nonlinear signal to PQ-encoded IPT via the RPU's ycc_to_rgb matrix
// and signal offsets. Applying the RPU offsets keeps integer offset codes
// exactly on sample codes (2^bits/(2^bits-1) folded in on the CPU).
float3 dovi_signal_to_pq_rgb(float3 sig) {
    float3 reshaped = dovi_reshaped_signal(sig) - dovi.dovi_nonlinear_offset.xyz;
    return float3(
        dot(dovi.dovi_nonlinear_matrix[0].xyz, reshaped),
        dot(dovi.dovi_nonlinear_matrix[1].xyz, reshaped),
        dot(dovi.dovi_nonlinear_matrix[2].xyz, reshaped)
    );
}

// Linearized BT.2020-referred HPE LMS back to linear RGB, using the composite
// of the fixed HPE inverse with the RPU's rgb_to_lms matrix (premultiplied on
// the CPU, matching libplacebo's dovi_lms2rgb).
float3 dovi_lms_to_rgb(float3 lin) {
    return float3(
        dot(dovi.dovi_lms_matrix[0].xyz, lin),
        dot(dovi.dovi_lms_matrix[1].xyz, lin),
        dot(dovi.dovi_lms_matrix[2].xyz, lin)
    );
}

VsOut vs_main(VsIn input) {
    VsOut output;
    output.position = float4(input.position, 0.0, 1.0);
    output.texcoord = input.texcoord;
    return output;
}

float packed_luma_texel(int2 virtual_coord_in, int2 virtual_size) {
    int2 virtual_coord = clamp(virtual_coord_in, int2(0, 0), virtual_size - int2(1, 1));
    int2 packed_coord = virtual_coord >> int2(1, 1);
    float4 packed = lumaTex.Load(int3(packed_coord, 0));
    uint component = uint((virtual_coord.y & 1) * 2 + (virtual_coord.x & 1));
    if (component == 0) {
        return packed.r;
    }
    if (component == 1) {
        return packed.g;
    }
    if (component == 2) {
        return packed.b;
    }
    return packed.a;
}

// Reconstruct bilinear sampling of the virtual 2W x 2H luma texture stored
// as packed TL/TR/BL/BR RGBA texels by the D3D11 ArtCNN compute pass.
float sample_packed_luma(float2 texcoord) {
    uint packed_width;
    uint packed_height;
    lumaTex.GetDimensions(packed_width, packed_height);
    int2 virtual_size = int2(packed_width, packed_height) * int2(2, 2);
    float2 sample_position = saturate(texcoord) * float2(virtual_size) - float2(0.5, 0.5);
    int2 lo = int2(floor(sample_position));
    float2 fraction = frac(sample_position);
    float y0 = lerp(
        packed_luma_texel(lo, virtual_size),
        packed_luma_texel(lo + int2(1, 0), virtual_size),
        fraction.x
    );
    float y1 = lerp(
        packed_luma_texel(lo + int2(0, 1), virtual_size),
        packed_luma_texel(lo + int2(1, 1), virtual_size),
        fraction.x
    );
    return lerp(y0, y1, fraction.y);
}

float4 ps_main(VsOut input) : SV_Target {
    uint base_input_mode = input_mode & 255u;
    bool packed_alpha = (input_mode & 256u) != 0u;
    float2 color_coord = packed_alpha
        ? float2(input.texcoord.x * 0.5, input.texcoord.y)
        : input.texcoord;
    float2 alpha_coord = float2(0.5 + input.texcoord.x * 0.5, input.texcoord.y);
    float2 luma_coord = color_coord * texture_scales.xy;
    float2 chroma_coord = color_coord * texture_scales.zw;
    float y_sample = base_input_mode == 2u
        ? sample_packed_luma(luma_coord)
        : lumaTex.Sample(videoSampler, luma_coord).r;
    float2 cbcr_sample = chromaTex.Sample(videoSampler, chroma_coord).rg;
    bool dovi_enabled = dovi.dovi_flags.x != 0.0;
    bool dovi_ycbcr_input = dovi_enabled && (base_input_mode == 0u || base_input_mode == 2u);
    float3 rgb;
    if (dovi_ycbcr_input) {
        // The base layer carries the raw 12-bit DV signal (10-bit container,
        // full range); range expansion and the YCbCr matrix are replaced by
        // the RPU reshaping + ycc_to_rgb path.
        float3 sig = float3(y_sample, cbcr_sample.x, cbcr_sample.y);
        if (is_p010 != 0u) {
            sig *= 65535.0 / 65472.0;
        }
        rgb = dovi_signal_to_pq_rgb(sig);
    } else {
        float y;
        float2 cbcr;
        expand_ycbcr_range(y_sample, cbcr_sample, y, cbcr);

        float kr = luma_coefficients.x;
        float kg = max(luma_coefficients.y, 0.000001);
        float kb = luma_coefficients.z;
        rgb.r = y + 2.0 * (1.0 - kr) * cbcr.y;
        rgb.b = y + 2.0 * (1.0 - kb) * cbcr.x;
        rgb.g = (y - kr * rgb.r - kb * rgb.b) / kg;
    }
    rgb = transfer_to_source_reference_linear(rgb);
    if (dovi_ycbcr_input) {
        rgb = dovi_lms_to_rgb(rgb);
    }
    rgb = source_reference_to_nits(rgb);
    rgb = tone_map_nits(rgb);
    rgb = target_nits_to_reference_linear(rgb);
    rgb = gamut_compress(rgb);
    rgb = target_reference_linear_to_output(rgb);
    float alpha = 1.0;
    if (packed_alpha) {
        float alpha_sample = lumaTex.Sample(
            videoSampler,
            alpha_coord * texture_scales.xy
        ).r;
        float unused_y;
        float2 unused_cbcr;
        expand_ycbcr_range(alpha_sample, float2(0.5, 0.5), unused_y, unused_cbcr);
        alpha = saturate(unused_y);
    }
    return final_output(rgb, alpha);
}

float4 encode_ps_main(VsOut input) : SV_Target {
    float3 rgb = lumaTex.Sample(videoSampler, input.texcoord).rgb;
    rgb = target_reference_linear_to_output(rgb);
    return final_output(rgb, 1.0);
}
"#;

const OVERLAY_SHADER_SOURCE: &[u8] = br#"
struct VsIn {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    float2 texcoord : TEXCOORD0;
};

cbuffer OverlayConstants : register(b0) {
    float4 rect;
    float4 tex_rect;
    float2 viewport;
    uint overlay_mode;
    uint reserved0;
    float4 color;
    float4 ui_nits;
};

Texture2D overlayTex : register(t0);
SamplerState overlaySampler : register(s0);

// SDR keeps its existing gamma-space composition. For HDR10, ui_nits.y is
// the scene target's reference white, so return reference-linear values and
// let the final encode pass apply PQ after fixed-function alpha blending.
float3 sdr_ui_color_to_target_output(float3 rgb) {
    if (ui_nits.y > 0.0) {
        float3 linear_rgb = pow(max(rgb, float3(0.0, 0.0, 0.0)), float3(2.2, 2.2, 2.2));
        return linear_rgb * (max(ui_nits.x, 1.0) / max(ui_nits.y, 1.0));
    }
    return rgb;
}

VsOut overlay_vs_main(VsIn input) {
    float2 pixel = rect.xy + input.texcoord * rect.zw;
    float2 safe_viewport = max(viewport, float2(1.0, 1.0));
    float2 ndc = float2(
        pixel.x / safe_viewport.x * 2.0 - 1.0,
        1.0 - pixel.y / safe_viewport.y * 2.0
    );
    VsOut output;
    output.position = float4(ndc, 0.0, 1.0);
    output.texcoord = tex_rect.xy + input.texcoord * tex_rect.zw;
    return output;
}

float4 overlay_ps_main(VsOut input) : SV_Target {
    float4 sampled = overlayTex.Sample(overlaySampler, input.texcoord);
    if (overlay_mode == 1u) {
        float3 rgb = sdr_ui_color_to_target_output(color.rgb);
        return float4(rgb, color.a * sampled.r);
    }
    sampled.rgb = sdr_ui_color_to_target_output(sampled.rgb);
    return sampled;
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct VideoVertex {
    position: [f32; 2],
    texcoord: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct D3d11VideoConstants {
    common: VideoUniforms,
    texture_scales: [f32; 4],
}

/// Overlay quad uniforms, byte-compatible with the HLSL `OverlayConstants`
/// cbuffer (80 bytes, 16-byte aligned). `ui_nits.x` is SDR UI white and
/// `ui_nits.y` is the target reference white when compositing scene-linear.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayUniforms {
    rect: [f32; 4],
    tex_rect: [f32; 4],
    viewport: [f32; 2],
    overlay_mode: u32,
    reserved0: u32,
    color: [f32; 4],
    ui_nits: [f32; 4],
}

impl OverlayUniforms {
    #[allow(clippy::too_many_arguments)]
    fn rgba_plane(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        Self {
            rect: [x as f32, y as f32, width as f32, height as f32],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 0,
            reserved0: 0,
            color: [1.0, 1.0, 1.0, 1.0],
            ui_nits: overlay_ui_nits(target),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn alpha_atlas(
        color_rgba: u32,
        place_x: i32,
        place_y: i32,
        place_w: u32,
        place_h: u32,
        atlas_x: u32,
        atlas_w: u32,
        atlas_h: u32,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        let color = AssColor::from_libass_rgba(color_rgba);
        let aw = atlas_w.max(1) as f32;
        let ah = atlas_h.max(1) as f32;
        Self {
            rect: [
                place_x as f32,
                place_y as f32,
                place_w as f32,
                place_h as f32,
            ],
            tex_rect: [
                atlas_x as f32 / aw,
                0.0,
                place_w as f32 / aw,
                place_h as f32 / ah,
            ],
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            reserved0: 0,
            color: [
                f32::from(color.red) / 255.0,
                f32::from(color.green) / 255.0,
                f32::from(color.blue) / 255.0,
                f32::from(color.alpha) / 255.0,
            ],
            ui_nits: overlay_ui_nits(target),
        }
    }

    fn alpha_atlas_rect(
        color: [f32; 4],
        rect: [f32; 4],
        tex_rect: [f32; 4],
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
    ) -> Self {
        Self {
            rect,
            tex_rect,
            viewport: [viewport_w.max(1) as f32, viewport_h.max(1) as f32],
            overlay_mode: 1,
            reserved0: 0,
            color,
            ui_nits: overlay_ui_nits(target),
        }
    }
}

fn overlay_ui_nits(target: TargetColorState) -> [f32; 4] {
    if matches!(target.transfer, TransferFunction::Pq) {
        let reference_white = target.reference_white_nits.max(1.0);
        [reference_white, reference_white, 0.0, 0.0]
    } else {
        [100.0, 0.0, 0.0, 0.0]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct D3d11RendererStats {
    pub surface_width: u32,
    pub surface_height: u32,
    pub rendered_frames: u64,
    pub software_video_frames: u64,
    pub hardware_video_frames: u64,
    pub zero_copy_video_frames: u64,
    pub direct_zero_copy_video_frames: u64,
    pub shared_handle_video_frames: u64,
    pub cpu_video_frame_fallbacks: u64,
    pub hdr_source_frames: u64,
    pub hdr10_output_frames: u64,
    pub sdr_tonemap_frames: u64,
    pub hdr10_metadata_updates: u64,
    pub hdr10_metadata_failures: u64,
    pub hdr10_output_failures: u64,
    pub hdr10_output_active: bool,
    /// 设备丢失（device lost/reset）累计次数：PresenterRuntime 每自愈一次 +1。
    pub device_lost_events: u64,
    /// 最近一次设备丢失的 `GetDeviceRemovedReason` HRESULT：`DXGI_ERROR_DEVICE_RESET`
    /// (0x887A0006) 是 TDR 驱动重置；`DXGI_ERROR_DEVICE_REMOVED` (0x887A0005)
    /// 是驱动/显存级错误。0 表示尚未记录。供 HUD/诊断快照区分根因类别。
    pub last_device_lost_reason: u32,
    pub import_failures: u64,
    pub prepared_overlay_frames: u64,
    pub prepared_overlay_subtitle_planes: u64,
    /// 叠加层平面纹理（字幕平面 + alpha 图集）的 `CreateTexture2D` 次数。
    ///
    /// 这是"每帧新建纹理"抖动的直接计数：缓存命中时它不增长。
    pub overlay_plane_texture_creates: u64,
    /// 叠加层平面纹理复用次数：命中缓存，只做一次 `UpdateSubresource`，
    /// 没有新建/释放纹理。
    pub overlay_plane_texture_reuses: u64,
    pub overlay_alpha_atlas_uploads: u64,
    pub overlay_alpha_atlas_reuses: u64,
    pub danmaku_passes: u64,
    pub danmaku_items: u64,
    pub attached: bool,
    // ===== UI 线程（宿主渲染回调）分段耗时 =====
    //
    // 这一组是**回答「一拍多少 ms 花在哪」的唯一直接证据**。存在的理由：
    // 宿主把 `render_tick` 挂在显示合成回调（WinUI `CompositionTarget::Rendering`）
    // 上，而那个回调与窗口消息（`WM_MOVING`/`WM_SIZING`/`WM_NCMOUSEMOVE`）**共用
    // 同一个 UI 线程**。所以这里每一项都是「窗口交互被推迟多久」的直接构成项：
    // 用户在拖窗口/缩放/最大化的瞬间感受到的卡顿，就是这些数字之和。
    //
    // 这些是 **CPU/提交侧**耗时（`Instant::now()` 差值），不是 GPU 执行耗时：
    // D3D11 的 immediate context 是异步提交，`Present1` 返回时 GPU 可能还在跑
    // 上一个命令流。要 GPU 真时间必须用 timestamp query，本后端**没有**实现
    // —— 所以 `RendererRuntimeStats::last_gpu_duration` 在 Windows/D3D11 上
    // 恒为 0（只有 Metal 后端赋值）。诊断面板上的 `gpu=` 在本平台因此不可信，
    // 看这一组。
    /// 最近一拍的 `prepare_overlay_draws` 耗时（字幕/HUD 平面准备）。
    pub last_overlay_prepare_duration: Duration,
    /// 最近一拍的 `prepare_danmaku_draws` 耗时（弹幕字形图集查找/光栅化 + 顶点构建）。
    ///
    /// 这是**候选热点**：CJK 字形首次出现要跑 freetype/harfbuzz 光栅化并上传
    /// 图集，图集未命中时会显著慢于命中。与真机日志「慢拍集中在前 600s 且单调
    /// 递减」的模式一致（字形逐渐被缓存）。
    pub last_danmaku_prepare_duration: Duration,
    /// 最近一拍的 `draw_video` 提交耗时（不含 GPU 执行）。
    pub last_draw_video_duration: Duration,
    /// 最近一拍的叠加层（字幕 + 弹幕）`draw_overlays` 提交耗时。
    pub last_draw_overlay_duration: Duration,
    /// 最近一拍的 `render_video` 总耗时（含上面各项 + present + 表面校验）。
    ///
    /// 与 `last_render_current_duration`（presenter 侧测的同一段）互为交叉校验：
    /// 两者应当接近相等，差得多说明慢在 `render_current_frame` 的外壳（设备丢失
    /// 检查等）。
    pub last_present_duration: Duration,
    /// `Present1` 真正提交进 DXGI 队列的累计次数。
    pub present_submitted: u64,
    /// `Present1` 返回 `DXGI_ERROR_WAS_STILL_DRAWING` 被丢弃的累计次数。
    ///
    /// **这是判定「present 是否在阻塞 UI 线程」的判决性计数器**：本后端刻意用
    /// `Present1(0, DXGI_PRESENT_DO_NOT_WAIT)`，队列满时丢弃本拍而不是等待。
    /// 所以正常情况下这个数应当持续增长（每拍或多拍一次），而
    /// `last_present_duration` 应当保持在**亚毫秒**量级。若真机上看到
    /// `present_submitted` 几乎不涨、`last_present_duration` 却是 10ms+，那才是
    /// 「present 同步等待 vsync」——此时再看 `present_dropped` 是否也在涨。
    pub present_dropped: u64,
    /// 最近一次 [`wait_for_gpu`] 视频帧交接栅栏的等待时长。
    ///
    /// **这是判定「UI 线程是否被无谓空等」的判决性数字**，也是本次修复的验证点。
    /// 历史实现用 `sleep(1ms)` 轮询，撞上 Windows 默认 15.625ms 时钟粒度 →
    /// 每个上屏视频帧白等 ≈15.5ms。修复后（阻塞 `GetData`）应当降到**亚毫秒**。
    ///
    /// 看这个数的方法：播放中让它保持 <1ms；若它又回到 10ms+ 量级，说明栅栏
    /// 重新变成了瓶颈（或被换回轮询实现）。
    pub last_wait_gpu_duration: Duration,
    /// [`wait_for_gpu`] 被调用的累计次数（≈ 上屏视频帧数）。
    pub wait_gpu_calls: u64,
    /// 其中等待超过 2ms 的次数。稳态播放时应**恒为 0**；持续增长即回归。
    pub slow_wait_gpu_count: u64,
    /// 视频帧交接栅栏的 `ID3D11Query` 对象的**实际 `CreateQuery` 次数**。
    ///
    /// 这是"每个上屏视频帧新建一个 query 再释放"是否被消掉的直接计数：复用
    /// 生效时它只在**设备重建**时 +1（一次连续播放通常不超过 2~3），不会随
    /// [`Self::wait_gpu_calls`] 同步增长。若它与 `wait_gpu_calls` 同阶增长，
    /// 说明 [`D3d11Renderer::wait_for_gpu`] 的复用槽被改回了"每次新建"。
    pub gpu_fence_query_creates: u64,
    /// 视频帧交接栅栏**复用既有 query** 的次数（只做 `End`/`GetData`，
    /// 没有 `CreateQuery`）。稳态播放时它应当 ≈ [`Self::wait_gpu_calls`]。
    pub gpu_fence_query_reuses: u64,
    // ===== present 的真实分布 + 合成器节拍 =====
    //
    // 存在的理由（真机日志实测，498 秒采样）：此前只有 `last_present_duration`
    // 一个**最后一个采样**，于是 0.52 / 1.88 / 0.32 这类单点容易被当成"平均
    // 0.5ms"。把这 498 个单点摊开后的真实分布是 p50=0.41ms、p90=1.11ms、
    // p99=14.55ms —— 单点既看不到中位数，也看不到那条 15ms 的尾。
    /// `Present1` 的累计调用次数。均值 = [`Self::present_total`] / 本值。
    pub present_calls: u64,
    /// `Present1` 的累计耗时。
    pub present_total: Duration,
    /// 单次 `Present1` 的最长耗时。
    pub present_max: Duration,
    /// `present` 耗时超过 1ms 的次数：「这一拍被 present 拖住」的计数。
    pub present_over_1ms: u64,
    /// `present` 耗时超过 8ms 的次数。
    ///
    /// 8ms 是 120Hz 的一帧（8.33ms）。这个计数**持续增长**说明 present 在
    /// **等合成器的一帧**：真机日志里存在整秒的 `n=60、present=12.4~15.8ms`
    /// 段落，那些秒里 UI 线程 67~92% 的时间花在 present 内，而同一行里的
    /// `准备/绘制` 仍是 0.00~0.29ms —— 不是我们在算，是在等。
    ///
    /// 反过来，只有在**本计数不增长**而 `present` 仍然高的时候，那份耗时才是
    /// 我们自己的提交成本（最典型的是叠加层平面上传在 flush 边界结算，见
    /// [`Self::overlay_plane_uploads`]）。
    pub present_over_8ms: u64,
    /// [`D3d11FrameStatsSampler`] 是否成功从 `GetFrameStatistics` 取到读数。
    pub frame_stats_available: bool,
    /// 采样窗口内**显示器的 vblank 速率**，单位**毫赫兹**（120000 = 120.000Hz）。
    ///
    /// 这是"面板此刻跑在几 Hz"的直接读数，用来区分两种完全不同的归因：
    /// * 掉到 60000 → 面板/系统真的切到了 60Hz（此时我们的代码无从优化）；
    /// * 仍是 120000 而 [`Self::presents_per_refresh_milli`] ≈ 500 → 面板仍是
    ///   120Hz，是合成器在按 60Hz 消费我们的帧。
    ///
    /// 用整数毫赫兹而不是 f32：本结构体与 [`RendererRuntimeStats`] 都派生
    /// `Eq`，引入浮点会把这个 derive 打掉（宿主与测试都在用 `assert_eq!`）。
    pub display_refresh_hz_milli: u32,
    /// 每个 vblank 我们提交了几帧，单位**千分之一**（1000 = 1.000 帧/vblank）。
    pub presents_per_refresh_milli: u32,
    /// 每个 vblank **真正上屏**了几帧，单位同上。
    ///
    /// 明显小于 [`Self::presents_per_refresh_milli`] 时，说明提交进去的帧被
    /// 合成器丢掉了（我们交了、没显示）。
    pub shown_frames_per_refresh_milli: u32,
    // ===== 叠加层平面上传（候选 C：内容未变就不传）=====
    /// 叠加层平面**真正执行** `UpdateSubresource` 的累计次数。
    ///
    /// 每次上传本身几乎不花时间，但它的 flush 会在下游结算 —— 实测（本机
    /// AMD Radeon 集显、未挂 XAML visual 的 composition swapchain、按 120Hz
    /// 节拍 present）：present 前插 1 次 768x64 RGBA（0.20MB）上传，present
    /// p50 从 240µs 涨到 339µs（+99µs）；插 1 次 1080p RGBA（8.29MB）上传，
    /// p50 涨到 1062µs（+822µs）。所以"少传一次"省的是 **present 里的时间**。
    pub overlay_plane_uploads: u64,
    /// 上传进 GPU 的叠加层平面总字节数。
    pub overlay_plane_upload_bytes: u64,
    /// 因内容与上次逐字节相同而**跳过**上传的次数。跳过的每一次都少付一次
    /// 上面那笔提交成本。
    pub overlay_plane_upload_skips: u64,
}

/// `IDXGISwapChain::GetFrameStatistics` 采样器：读出**显示器的 vblank 速率**与
/// 「每个 vblank 我们提交/上屏了几帧」。
///
/// 一手依据（Microsoft Learn）：
/// * `IDXGISwapChain::GetFrameStatistics` 的 Remarks 原文："You can only use
///   GetFrameStatistics for swap chains that either use the flip presentation
///   model or draw in full-screen mode." —— 本后端走
///   `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`，所以窗口化下可用（该页同时写明
///   "Statistics are not reliable in many multiple monitor scenarios, as well as
///   scenarios where other fullscreen apps are running. It may be necessary to
///   call DwmFlush before frame statistics are updated on systems that support
///   Hardware Flip Queue."）。
/// * `DXGI_FRAME_STATISTICS` 的字段语义：`PresentCount` = "running total count
///   of times that an image was presented to the monitor"（**不是**我们调用
///   `Present1` 的次数）；`PresentRefreshCount` = "running total count of
///   v-blanks at which the last image was presented to the monitor"；
///   `SyncRefreshCount` = "running total count of v-blanks when the scheduler
///   last sampled the machine time"。
///
/// 因此 `ΔSyncRefreshCount / Δt` 就是**显示器此刻的刷新率**（与我们的提交
/// 行为无关），而 `ΔPresentCount / ΔSyncRefreshCount` 是"每个 vblank 我们
/// 交了几帧"。两者一起才能区分"面板掉到 60Hz"与"面板 120Hz 但合成器压我们"。
///
/// 采样周期固定为 [`FRAME_STATS_SAMPLE_TICKS`] 拍（120 拍 ≈ 1 秒），只做趋势
/// 判读 —— 上面那条 "not reliable" 的限制决定了它不能用来做逐帧断言。
struct D3d11FrameStatsSampler {
    last: DXGI_FRAME_STATISTICS,
    last_at: Instant,
    ticks: u32,
}

/// 每 120 拍采一次 `GetFrameStatistics`（120Hz 下约 1 秒）。
const FRAME_STATS_SAMPLE_TICKS: u32 = 120;

/// `present` 超过它就算「本拍被 present 拖住」（1ms）。
const PRESENT_SLOW_THRESHOLD: Duration = Duration::from_millis(1);
/// `present` 超过它按「在等合成器一帧」计：120Hz 一帧 8.33ms，取 8ms。
const PRESENT_COMPOSITOR_WAIT_THRESHOLD: Duration = Duration::from_millis(8);

struct D3d11DeviceState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    encode_pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    vertex_buffer: ID3D11Buffer,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    overlay_vertex_shader: ID3D11VertexShader,
    overlay_pixel_shader: ID3D11PixelShader,
    overlay_constants: ID3D11Buffer,
    overlay_sampler: ID3D11SamplerState,
    overlay_blend: ID3D11BlendState,
}

struct AttachedSurface {
    hwnd: HWND,
    /// WinUI 3 `SwapChainPanel` 嵌入模式：Some 时经 composition swap chain
    /// 渲染进 XAML 树（`raw_window` 传入 `ISwapChainPanelNative*`）。
    /// 裸指针：接口由宿主（SwapChainPanel）持有，生命周期长于渲染器。
    panel: Option<*mut c_void>,
    composition: bool,
    flutter_texture: bool,
    metrics: SurfaceMetrics,
    output_mode: D3d11OutputMode,
    swapchain: Option<IDXGISwapChain1>,
    output_texture: Option<ID3D11Texture2D>,
    // Immutable, GPU-complete snapshot exported to Flutter. Never render into
    // this resource again: opening its handle is not GPU consumer completion.
    published_texture: Option<ID3D11Texture2D>,
    render_target: Option<ID3D11RenderTargetView>,
    linear_render_target: Option<ID3D11RenderTargetView>,
    linear_shader_resource: Option<ID3D11ShaderResourceView>,
}

impl AttachedSurface {
    fn targets_ready(&self) -> bool {
        (if self.flutter_texture {
            self.output_texture.is_some()
        } else {
            self.swapchain.is_some()
        }) && self.render_target.is_some()
            && (!matches!(self.output_mode, D3d11OutputMode::Hdr10)
                || (self.linear_render_target.is_some() && self.linear_shader_resource.is_some()))
    }
}

struct ImportedVideoFrame {
    _frame: Option<Frame>,
    _texture: ID3D11Texture2D,
    _chroma_texture: Option<ID3D11Texture2D>,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    tex_rect: D3d11TexRect,
    _array_index: u32,
    frame_token: u64,
    constants: VideoUniforms,
}

struct FlutterTextureScene {
    frame_token: u64,
    generation: u64,
    upscaler: LumaUpscalerMode,
    overlay: Option<OverlayFrame>,
    danmaku: Option<DanmakuRenderPlan>,
}

impl FlutterTextureScene {
    fn new(frame_token: u64, upscaler: LumaUpscalerMode, context: RenderFrameContext<'_>) -> Self {
        Self {
            frame_token,
            generation: context.generation,
            upscaler,
            overlay: context.overlay.cloned(),
            danmaku: context.danmaku.cloned(),
        }
    }

    fn matches(
        &self,
        frame_token: u64,
        upscaler: LumaUpscalerMode,
        context: RenderFrameContext<'_>,
    ) -> bool {
        self.frame_token == frame_token
            && self.generation == context.generation
            && self.upscaler == upscaler
            && self.overlay_matches(context.overlay)
            && self.danmaku_matches(context.danmaku)
    }

    fn overlay_matches(&self, overlay: Option<&OverlayFrame>) -> bool {
        match (self.overlay.as_ref(), overlay) {
            (Some(a), Some(b)) => {
                a.viewport == b.viewport
                    && a.subtitle_planes == b.subtitle_planes
                    && a.subtitle_alpha_planes == b.subtitle_alpha_planes
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn danmaku_matches(&self, danmaku: Option<&DanmakuRenderPlan>) -> bool {
        match (self.danmaku.as_ref(), danmaku) {
            (Some(a), Some(b)) => {
                a.viewport == b.viewport && a.atlas == b.atlas && a.items == b.items
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn update(
        &mut self,
        frame_token: u64,
        upscaler: LumaUpscalerMode,
        context: RenderFrameContext<'_>,
    ) {
        // A new video frame need not copy unchanged subtitle/HUD pixels or
        // danmaku instances. Retain each cached buffer until its own content
        // changes; keep exact comparisons rather than lossy fingerprints.
        if !self.overlay_matches(context.overlay) {
            self.overlay = context.overlay.cloned();
        }
        if !self.danmaku_matches(context.danmaku) {
            self.danmaku = context.danmaku.cloned();
        }
        self.frame_token = frame_token;
        self.generation = context.generation;
        self.upscaler = upscaler;
    }
}

#[derive(Debug, Clone, Copy)]
struct D3d11DrawRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct D3d11TexRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl D3d11TexRect {
    const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    fn visible_region(
        visible_width: u32,
        visible_height: u32,
        texture_width: u32,
        texture_height: u32,
    ) -> Self {
        let texture_width = texture_width.max(1);
        let texture_height = texture_height.max(1);
        Self {
            x: 0.0,
            y: 0.0,
            width: visible_width.max(1).min(texture_width) as f32 / texture_width as f32,
            height: visible_height.max(1).min(texture_height) as f32 / texture_height as f32,
        }
    }

    fn right(self) -> f32 {
        (self.x + self.width).min(1.0)
    }

    fn bottom(self) -> f32 {
        (self.y + self.height).min(1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum D3d11OutputMode {
    #[default]
    Sdr,
    Hdr10,
}

impl D3d11OutputMode {
    fn swapchain_format(self) -> DXGI_FORMAT {
        match self {
            Self::Sdr => SDR_SWAPCHAIN_FORMAT,
            Self::Hdr10 => HDR10_SWAPCHAIN_FORMAT,
        }
    }

    fn color_space(self) -> DXGI_COLOR_SPACE_TYPE {
        match self {
            Self::Sdr => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            Self::Hdr10 => DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        }
    }

    fn target_color(self) -> TargetColorState {
        match self {
            Self::Sdr => TargetColorState::sdr(ColorPrimaries::Bt709),
            Self::Hdr10 => TargetColorState::hdr10(ColorPrimaries::Bt2020),
        }
    }

    fn target_color_for_source(self, source: SourceColorState) -> TargetColorState {
        match self {
            Self::Sdr if source.is_hdr() => {
                TargetColorState::sdr_tone_map_target(ColorPrimaries::Bt709)
            }
            _ => self.target_color(),
        }
    }
}

#[derive(Clone)]
struct D3d11OverlayTexture {
    _texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
}

struct D3d11OverlayDraw {
    texture: D3d11OverlayTexture,
    constants: OverlayUniforms,
}

struct D3d11DanmakuAtlasCache {
    version: u64,
    width: u32,
    height: u32,
    stride: usize,
    fill: D3d11OverlayTexture,
    outline: D3d11OverlayTexture,
}

impl D3d11DanmakuAtlasCache {
    fn can_reuse_for(&self, atlas: &DanmakuGlyphAtlas) -> bool {
        self.version == atlas.version
            && self.width == atlas.width
            && self.height == atlas.height
            && self.stride == atlas.stride
    }
}

/// 叠加层平面纹理缓存的条目上限（张）。
///
/// 一条字幕的包围盒在它的显示期内是稳定的（libass 只在文本/样式变化时改
/// 包围盒），所以「格式 + 尺寸」就是稳定的命中键。常见并发规模是 1 条原文 +
/// 1 条译文 + 偶尔的屏幕字；6 张足够覆盖。超出的平面退回按帧新建/释放
/// （修改前的行为），不会因为缓存不够而画错。
const OVERLAY_TEXTURE_CACHE_CAPACITY: usize = 6;

/// 叠加层平面纹理缓存的常驻字节上限。
///
/// 文本字幕平面很小（1080P 单行 ≈ 0.1MB），但位图字幕（PGS/VobSub）的平面
/// 可以整屏大（1080P RGBA ≈ 8MB）。按字节封顶，保证"复用"不会退化成"把整屏
/// 平面钉死在显存里"；单张超过预算的平面不进缓存。
const OVERLAY_TEXTURE_CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// 缓存条目的形状与使用信息。与 D3D 资源分离，让淘汰策略可以单独单测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct D3d11OverlayTextureSlot {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bytes: usize,
    /// 最近一次使用它的渲染帧序号，用于 LRU 与"本帧是否已交出"判定。
    last_frame: u64,
}

/// 本帧这张平面该走哪条路径。见 `plan_overlay_texture_slot`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum D3d11OverlayTexturePlan {
    /// 命中第 `index` 条：`UpdateSubresource` 复用，不新建。
    Reuse(usize),
    /// 第 `index` 条是空位：新建并缓存。
    Insert(usize),
    /// 淘汰第 `index` 条（LRU）后新建并缓存。
    Replace(usize),
    /// 不进缓存：本帧新建、帧末释放（容量或字节预算不允许）。
    Transient,
}

/// 叠加层平面纹理的「格式 + 尺寸」小 LRU 缓存。
///
/// 修改前：`prepare_overlay_draws` 对每个字幕平面、每一帧都调
/// `create_cpu_texture`（`CreateTexture2D` + `CreateShaderResourceView`），
/// 帧末随 `draws` 一起释放。1080P 单行字幕平面约 0.1MB，60fps 下就是约
/// 6MB/s 的分配/释放抖动，WDDM 下每次都是驱动级分配。
///
/// 现在：同格式同尺寸的平面命中条目，只做一次 `UpdateSubresource`，纹理对象
/// （含已建好的 SRV）跨帧复用；常驻被 `OVERLAY_TEXTURE_CACHE_BUDGET_BYTES`
/// 与 `OVERLAY_TEXTURE_CACHE_CAPACITY` 双重封顶。
///
/// 同一帧内已交出去的条目不会再被命中（`last_frame == frame` 判定）：一帧里
/// 两张尺寸相同的平面必须各自持有一张纹理，否则后一次上传会覆盖前一张的
/// 像素。容量不够时新平面走 `Transient`，即修改前的行为。
///
/// **设备绑定**：条目里的纹理属于创建它的 D3D11 设备，任何设备切换/重建
/// （`set_device`）都必须清空，否则会拿到旧设备的纹理。
struct D3d11OverlayTextureCache {
    /// 当前渲染帧序号，`begin_frame` 每帧 +1。
    frame: u64,
    entries: Vec<D3d11OverlayTextureEntry>,
    cached_bytes: usize,
}

struct D3d11OverlayTextureEntry {
    slot: D3d11OverlayTextureSlot,
    texture: D3d11OverlayTexture,
    /// 这张纹理当前内容的副本；`None` = 内容未知，必须重传。
    ///
    /// 存在的理由（实测，不是推测）：`UpdateSubresource` 的成本**不在调用点**，
    /// 而在下游 flush 边界结算。本机（AMD Radeon 集显、composition swapchain、
    /// 按 120Hz 节拍 present）实测：present 前插 1 次 768x64 RGBA（0.20MB）上传，
    /// present p50 240µs → 339µs（+99µs）；插 1 次 1080p RGBA（8.29MB），p50 →
    /// 1062µs（+822µs）—— **单次调用的成本几乎与字节数无关**。
    ///
    /// 而字幕在"同一句字幕期间"逐拍逐字节相同：实测同一句静态字幕连续 120 拍
    /// （libass 的 2 张 alpha 位图 828x57 / 47,420 字节，以及 debug 渲染器的
    /// 360x28 RGBA 平面）**119/119 完全相同**。逐字节比对 40KB 实测 p50 1.5µs，
    /// 比无条件重传便宜约两个数量级。
    ///
    /// 正确性边界：条目被 `Insert`/`Replace` 换入新纹理时同步写入内容（初始数据
    /// 也在创建时上传）；`clear()`（设备重建）、`Transient`（不入缓存）都不会
    /// 留下"内容已知"的假象 —— 这两条路径必须重新上传，否则静态字幕会在纹理被
    /// 驱逐/重建后消失。
    content: Option<Vec<u8>>,
}

impl D3d11OverlayTextureCache {
    const fn new() -> Self {
        Self {
            frame: 0,
            entries: Vec::new(),
            cached_bytes: 0,
        }
    }

    /// 渲染帧开始：帧序号 +1。同一帧内的"已交出"判定全部与它比较。
    fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// 设备重建：丢掉所有属于旧设备的纹理。
    fn clear(&mut self) {
        self.entries.clear();
        self.cached_bytes = 0;
    }

    /// 取一张写有 `data` 的纹理，能复用就复用。
    ///
    /// `bytes_per_row` 与 `height` 一起决定这张平面的字节数（缓存预算用它计量）。
    fn acquire(
        &mut self,
        state: &D3d11DeviceState,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        data: &[u8],
        bytes_per_row: u32,
        stats: &mut D3d11RendererStats,
    ) -> Result<D3d11OverlayTexture> {
        let bytes = (bytes_per_row.max(1) as usize).saturating_mul(height.max(1) as usize);
        let mut slots = [None; OVERLAY_TEXTURE_CACHE_CAPACITY];
        let occupied = self.entries.len().min(OVERLAY_TEXTURE_CACHE_CAPACITY);
        for (index, entry) in self.entries.iter().take(occupied).enumerate() {
            slots[index] = Some(entry.slot);
        }
        match plan_overlay_texture_slot(
            &slots[..occupied],
            self.cached_bytes,
            width,
            height,
            format,
            bytes,
            self.frame,
        ) {
            D3d11OverlayTexturePlan::Reuse(index) => {
                let entry = &mut self.entries[index];
                entry.slot.last_frame = self.frame;
                stats.overlay_plane_texture_reuses += 1;
                // 内容与这张纹理里已有的一致 → 跳过 `UpdateSubresource`。
                // 跳过的是**下游 flush 里那笔 ~100µs 的提交成本**（见
                // `D3d11OverlayTextureEntry::content` 的实测数据），不是省一次
                // 内存拷贝。
                if entry.content.as_deref() == Some(data) {
                    stats.overlay_plane_upload_skips += 1;
                    return Ok(entry.texture.clone());
                }
                upload_overlay_texture(state, &entry.texture._texture, data, bytes_per_row);
                entry.content = Some(data.to_vec());
                stats.overlay_plane_uploads += 1;
                stats.overlay_plane_upload_bytes += data.len() as u64;
                Ok(entry.texture.clone())
            }
            D3d11OverlayTexturePlan::Insert(index) => {
                let texture =
                    create_cpu_texture(state, width, height, format, data, bytes_per_row)?;
                let slot = D3d11OverlayTextureSlot {
                    width,
                    height,
                    format,
                    bytes,
                    last_frame: self.frame,
                };
                if index < self.entries.len() {
                    // 条目一旦写满就只会走 Replace；这里只是防御性兜底。
                    let replaced = mem::replace(
                        &mut self.entries[index],
                        D3d11OverlayTextureEntry {
                            slot,
                            texture: texture.clone(),
                            content: Some(data.to_vec()),
                        },
                    );
                    self.cached_bytes = self
                        .cached_bytes
                        .saturating_sub(replaced.slot.bytes)
                        .saturating_add(bytes);
                } else {
                    self.entries.push(D3d11OverlayTextureEntry {
                        slot,
                        texture: texture.clone(),
                        content: Some(data.to_vec()),
                    });
                    self.cached_bytes = self.cached_bytes.saturating_add(bytes);
                }
                stats.overlay_plane_texture_creates += 1;
                Ok(texture)
            }
            D3d11OverlayTexturePlan::Replace(index) => {
                let texture =
                    create_cpu_texture(state, width, height, format, data, bytes_per_row)?;
                let replaced = mem::replace(
                    &mut self.entries[index],
                    D3d11OverlayTextureEntry {
                        slot: D3d11OverlayTextureSlot {
                            width,
                            height,
                            format,
                            bytes,
                            last_frame: self.frame,
                        },
                        texture: texture.clone(),
                        content: Some(data.to_vec()),
                    },
                );
                self.cached_bytes = self
                    .cached_bytes
                    .saturating_sub(replaced.slot.bytes)
                    .saturating_add(bytes);
                stats.overlay_plane_texture_creates += 1;
                Ok(texture)
            }
            D3d11OverlayTexturePlan::Transient => {
                stats.overlay_plane_texture_creates += 1;
                create_cpu_texture(state, width, height, format, data, bytes_per_row)
            }
        }
    }
}

/// 选出本帧这张平面该用哪条缓存路径。纯函数：只读元信息，便于单测。
///
/// 规则（顺序即优先级）：
/// 1. 本帧还没交出去的「格式 + 尺寸」相同条目 → `Reuse`；
/// 2. 单张就超过字节预算 → `Transient`（避免一张整屏位图把显存钉死）；
/// 3. 还有空位且加进去不超预算 → `Insert`；
/// 4. 淘汰本帧未使用的最旧条目（LRU），换入后不超预算 → `Replace`；
/// 5. 其余 → `Transient`（同帧同尺寸平面多于容量时的兜底，行为同修改前）。
fn plan_overlay_texture_slot(
    slots: &[Option<D3d11OverlayTextureSlot>],
    cached_bytes: usize,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bytes: usize,
    frame: u64,
) -> D3d11OverlayTexturePlan {
    let occupied = || {
        slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|slot| (index, slot)))
    };
    if let Some((index, _)) = occupied().find(|(_, slot)| {
        slot.last_frame != frame
            && slot.width == width
            && slot.height == height
            && slot.format == format
    }) {
        return D3d11OverlayTexturePlan::Reuse(index);
    }
    if bytes > OVERLAY_TEXTURE_CACHE_BUDGET_BYTES {
        return D3d11OverlayTexturePlan::Transient;
    }
    let used = occupied().count();
    if used < OVERLAY_TEXTURE_CACHE_CAPACITY
        && cached_bytes.saturating_add(bytes) <= OVERLAY_TEXTURE_CACHE_BUDGET_BYTES
    {
        // 条目按插入顺序紧凑存放，空位就是最后一条之后。
        return D3d11OverlayTexturePlan::Insert(used);
    }
    let victim = occupied()
        .filter(|(_, slot)| slot.last_frame != frame)
        .min_by_key(|(index, slot)| (slot.last_frame, *index));
    if let Some((index, slot)) = victim {
        if cached_bytes
            .saturating_sub(slot.bytes)
            .saturating_add(bytes)
            <= OVERLAY_TEXTURE_CACHE_BUDGET_BYTES
        {
            return D3d11OverlayTexturePlan::Replace(index);
        }
    }
    D3d11OverlayTexturePlan::Transient
}

/// `append_alpha_atlas_draws` 重建 alpha 图集时的 CPU 侧缓冲，跨帧复用。
///
/// 修改前每拍 `vec![0u8; atlas_w * atlas_h]` + `Vec::new()`（placements），
/// 帧末整块释放。图集形状由这一拍参与拼接的平面尺寸决定，而一句字幕的显示期内
/// 它是稳定的，于是每拍都在为同一形状付一次「分配 + 释放」。
///
/// **别名判断（为什么复用不会改坏输出）**：这块缓冲只以 `&[u8]` 交给
/// `D3d11OverlayTextureCache::acquire`，两条出口都不保留指针 —— 命中缓存走
/// `ID3D11DeviceContext::UpdateSubresource`，未命中走 `ID3D11Device::CreateTexture2D`
/// 的 `pInitialData`。两者都只在调用期间读取调用方内存：本机 WARP 实测见
/// `d3d11_overlay_upload_does_not_retain_source_buffer`（调用返回后**立刻改写**
/// 源缓冲，读回纹理仍是调用时的字节），所以返回后缓冲即可改写。缓冲也不会
/// 跨线程或跨帧逃逸：它只在本渲染器的这一次调用里被借用一次。
///
/// **实测成本与量级**（release，本机 2026-09-15，Windows 进程堆；跑法
/// `cargo test --release --lib alpha_atlas_rebuild_cost_is_measured -- --nocapture`）：
/// 同一形状（图集 1656x57 = 94,392 字节，2 张 828x57 / stride 832）下，
/// 复用后的重建 p50 ≈ 2.3µs、多轮实测 p95 ≤ 2.9µs；每拍新建缓冲的同一段
/// min ≈ 2.5µs、p50 2.6–3.9µs，同一台机器上有其它负载时 p50 会到 5–36µs、p95 45µs。
/// 即：**消除稳态下每拍一次堆分配；实测收益 0.2~0.3µs/拍（120 拍/s 折 0.03ms/s，
/// 约 0.003% 单核时间），主要价值是消除负载下的分配抖动**。
///
/// **这不是性能修复**：稳态收益小到会被噪声淹没（前序估计的 10–20µs/拍实测不成立），
/// 它只是把热路径上的每拍分配清掉。能稳定拿到的是「成本恒定」：复用后的 p50/p95
/// 在机器有其它负载时也不动（上面对照组是同一台机器、同一时段的数字）。
/// 分配器是宿主进程的全局分配器（bili-rs 用 mimalloc），换分配器会改变绝对数字。
#[derive(Default)]
struct D3d11AlphaAtlasScratch {
    /// 最近一次成功拼接出的图集宽度（像素）。
    width: usize,
    /// 最近一次成功拼接出的图集高度（像素）。
    height: usize,
    /// 图集像素缓冲，长度 ≥ `width * height`；形状缩小时不缩容，
    /// 免得窗口 resize 反复分配。
    pixels: Vec<u8>,
    /// 参与拼接的平面（`bitmaps` 下标）与它在图集里的起始 x。
    placements: Vec<(usize, usize)>,
}

impl D3d11AlphaAtlasScratch {
    /// 按 `bitmaps` 重拼图集，复用内部缓冲；`Ok(false)` = 没有可拼的平面
    /// （调用方直接返回，不画）。出错返回时 `pixels` 内容无意义，但
    /// `width`/`height` 仍是上一次成功拼接的形状。
    ///
    /// **等价性契约**：对同一 `bitmaps`，本函数产出的
    /// `pixels[..width * height]` 与「`vec![0u8; width * height]` 起手、再按
    /// 修改前的逐行 `copy_from_slice` 填充」逐字节相同 —— 无论这块缓冲此前
    /// 装过什么（别的尺寸的图集、被写脏的字节都算）。
    ///
    /// 契约里唯一容易破的点：图集里存在**任何一次拷贝都写不到**的字节。
    /// 某项高度小于 `atlas_height` 时，它下方各行的这一列区间就是这类字节：
    /// 新建缓冲里它们是 0，复用缓冲里则可能残留上一拍的像素。所以下面的整块
    /// 清零不是可选优化，而是维持输出逐字节等价的前提。测试：
    /// `alpha_atlas_scratch_reuses_buffer_with_identical_bytes`。
    fn rebuild(&mut self, bitmaps: &[crate::subtitle::SubtitleAlphaBitmap]) -> Result<bool> {
        let mut width = 0usize;
        let mut height = 0usize;
        for bitmap in bitmaps {
            if bitmap.placement.width == 0 || bitmap.placement.height == 0 {
                continue;
            }
            width += bitmap.placement.width as usize;
            height = height.max(bitmap.placement.height as usize);
        }
        if width == 0 || height == 0 {
            return Ok(false);
        }

        let len = width * height;
        if self.pixels.len() < len {
            self.pixels.resize(len, 0);
        }
        self.pixels[..len].fill(0);

        {
            let (pixels, placements) = (&mut self.pixels, &mut self.placements);
            let pixels = &mut pixels[..len];
            placements.clear();
            let mut cursor_x = 0usize;
            for (index, bitmap) in bitmaps.iter().enumerate() {
                let bw = bitmap.placement.width as usize;
                let bh = bitmap.placement.height as usize;
                if bw == 0 || bh == 0 {
                    continue;
                }
                if !bitmap.is_valid() {
                    return Err(PlayerError::Renderer(format!(
                        "d3d11: overlay alpha bitmap has {} bytes, expected at least {} for {}x{} stride {}",
                        bitmap.alpha.len(),
                        bitmap.required_len(),
                        bitmap.placement.width,
                        bitmap.placement.height,
                        bitmap.stride
                    )));
                }
                for row in 0..bh {
                    let src = row * bitmap.stride;
                    let dst = row * width + cursor_x;
                    pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
                }
                placements.push((index, cursor_x));
                cursor_x += bw;
            }
        }
        self.width = width;
        self.height = height;
        Ok(true)
    }
}

/// 把 CPU 像素写进已存在的叠加层纹理（等价于 `CreateTexture2D` 的初始数据，
/// 但省掉了纹理的创建与释放）。
fn upload_overlay_texture(
    state: &D3d11DeviceState,
    texture: &ID3D11Texture2D,
    data: &[u8],
    bytes_per_row: u32,
) {
    unsafe {
        state.context.UpdateSubresource(
            texture,
            0,
            None,
            data.as_ptr().cast::<c_void>(),
            bytes_per_row,
            0,
        );
    }
}

pub struct D3d11Renderer {
    state: Option<D3d11DeviceState>,
    surface: Option<AttachedSurface>,
    current_video: Option<ImportedVideoFrame>,
    flutter_scene: Option<FlutterTextureScene>,
    danmaku_atlas_cache: Option<D3d11DanmakuAtlasCache>,
    /// 字幕平面 / alpha 图集的小 LRU 纹理缓存：同尺寸平面跨帧复用纹理，
    /// 消除每帧 `CreateTexture2D` + 帧末释放的分配抖动。设备重建时清空。
    overlay_textures: D3d11OverlayTextureCache,
    /// alpha 图集的 CPU 侧拼接缓冲（见 [`D3d11AlphaAtlasScratch`]）。
    /// 纯 CPU 数据，与 D3D 设备无关，`set_device` 无需清空它。
    alpha_atlas_scratch: D3d11AlphaAtlasScratch,
    requested_output_mode: crate::renderer::output::OutputMode,
    video_alpha_mode: VideoAlphaMode,
    upscaler_mode: LumaUpscalerMode,
    upscaler: D3d11ArtCnn,
    next_frame_token: u64,
    hdr10_output_unavailable: bool,
    /// 设备丢失标记：GPU TDR（通常返回 `DXGI_ERROR_DEVICE_RESET`）或硬件错误
    /// （`DXGI_ERROR_DEVICE_REMOVED`）后，所有设备操作失败。由
    /// `render_current_frame` 检测置位，`PresenterRuntime` 据此触发自愈。
    device_lost: bool,
    /// 视频帧交接栅栏（[`Self::wait_for_gpu`]）复用的 `D3D11_QUERY_EVENT` query。
    ///
    /// 存在的理由与"为什么单个槽就够"见 `wait_for_gpu` 的文档。query 由创建它的
    /// 设备拥有，所以 [`Self::set_device`] 必须丢弃它（旧设备 TDR 后更是如此）。
    gpu_fence_query: Option<ID3D11Query>,
    /// present/合成器节拍采样槽（[`D3d11FrameStatsSampler`]）。
    ///
    /// 属于某个 swapchain 的一次采样窗口；换面/重建（`attach_surface`、
    /// `recreate_surface_targets`）时丢弃重来，否则会把旧链的 vblank 计数与
    /// 新链的计数相减，得到一个纯属虚构的刷新率。
    present_frame_stats: Option<D3d11FrameStatsSampler>,
    /// Cached perceptual gamut LUT (3D RGBA16F SRV), keyed the same way as
    /// the wgpu/Metal caches: (source, target, target black PQ, target peak PQ).
    gamut_lut: Option<(u32, u32, u32, u32, ID3D11ShaderResourceView)>,
    /// Background generation for a cache miss; the fast `gamut_compress`
    /// path renders until the LUT lands.
    gamut_lut_job: Option<GamutLutJob>,
    stats: D3d11RendererStats,
    /// 尺寸诊断节流键：上次记录的视频帧/backbuffer/target_rect 尺寸。
    /// 任一变化时打一条 `[size-diag]` 日志，避免每帧刷屏。
    last_size_diag: Option<(u32, u32, u32, u32, u32, u32, u32, u32)>,
}

// D3D11 设备/交换链/纹理均为自由线程 COM 对象；渲染器由宿主单线程独占访问
// （bili-rs 将 PresenterRuntime 移入后台播放线程，UI 线程不再触碰）。
unsafe impl Send for D3d11Renderer {}

impl D3d11Renderer {
    pub fn new() -> Result<Self> {
        Self::with_config(MetalRendererConfig::default())
    }

    pub fn with_config(config: MetalRendererConfig) -> Result<Self> {
        Ok(Self {
            state: None,
            surface: None,
            current_video: None,
            flutter_scene: None,
            danmaku_atlas_cache: None,
            overlay_textures: D3d11OverlayTextureCache::new(),
            alpha_atlas_scratch: D3d11AlphaAtlasScratch::default(),
            requested_output_mode: config.output_mode,
            video_alpha_mode: config.video_alpha_mode,
            upscaler_mode: config.luma_upscaler,
            upscaler: D3d11ArtCnn::default(),
            next_frame_token: 0,
            hdr10_output_unavailable: false,
            device_lost: false,
            gpu_fence_query: None,
            present_frame_stats: None,
            gamut_lut: None,
            gamut_lut_job: None,
            stats: D3d11RendererStats::default(),
            last_size_diag: None,
        })
    }

    pub fn stats(&self) -> D3d11RendererStats {
        self.stats
    }

    fn retire_current_video(&mut self) -> Result<()> {
        // Preserve main's HWND playback policy. Only compositor-owned surfaces
        // need this additional handoff barrier. Do not grow the decoder pool or
        // accumulate an unbounded queue of AVFrames on delayed/failed fences.
        if self.current_video.is_some()
            && self
                .surface
                .as_ref()
                .is_some_and(|s| s.composition || s.flutter_texture)
        {
            // 这里的等待时长是**头号诊断指标**：它跑在宿主的 UI 渲染回调线程
            // （WinUI `CompositionTarget::Rendering`）上，与 `WM_MOVING`/
            // `WM_SIZING` 共用同一个线程。历史上它因为 `sleep(1ms)` 轮询撞上
            // 15.625ms 的系统时钟粒度，每导入一个视频帧白等约 15.5ms —— 这就是
            // 「拖动/缩放窗口不跟手、120Hz 屏拉不满」的直接来源。改动见
            // `wait_for_gpu` 的文档。修复后本字段应当在**亚毫秒**量级。
            let waited = self.wait_for_gpu()?;
            self.stats.last_wait_gpu_duration = waited;
            if is_slow_gpu_wait(waited) {
                self.stats.slow_wait_gpu_count += 1;
            }
            self.stats.wait_gpu_calls += 1;
        }
        self.current_video = None;
        Ok(())
    }

    fn publish_flutter_texture(&mut self) -> Result<()> {
        if !self
            .surface
            .as_ref()
            .expect("surface ensured")
            .flutter_texture
        {
            return Ok(());
        }
        // 快照创建 + 拷贝放在一个内层作用域里完成：下面 `wait_for_gpu` 需要
        // `&mut self`（复用槽挂在 `self` 上），不能在 `surface`/`state` 还活着时调用。
        let snapshot = {
            let state = self.state.as_ref().expect("device ensured");
            let surface = self.surface.as_ref().expect("surface ensured");
            let source = surface.output_texture.as_ref().expect("texture ensured");
            let extent = surface.metrics.physical_extent;
            let (snapshot, _) =
                create_flutter_texture_target(&state.device, extent.width, extent.height)?;
            unsafe {
                state.context.CopyResource(&snapshot, source);
            }
            snapshot
        };
        // Flush only submits work. Complete the copy before another D3D device
        // opens this immutable snapshot. The extra GPU copy is texture-only.
        // 返回值（等待时长）只在 composition 主路径记入诊断，这里丢弃：flutter
        // 纹理不是 WinUI 宿主路径，混进同一个字段会让主路径的数字不可读。
        let _ = self.wait_for_gpu()?;
        self.surface
            .as_mut()
            .expect("surface ensured")
            .published_texture = Some(snapshot);
        Ok(())
    }

    /// 等 GPU 执行到当前命令队列末尾，用作「交还解码器视频帧」的栅栏。
    ///
    /// 返回本次实际等待时长（供诊断；见 [`D3d11RendererStats::last_wait_gpu_duration`]）。
    /// 设备尚未建立时返回 [`Duration::ZERO`]（没有命令队列可等，与历史行为一致）。
    ///
    /// # 为什么不能用 `sleep` 轮询（曾经的实现，已修）
    ///
    /// 旧实现在这里 `std::thread::sleep(Duration::from_millis(1))` 轮询
    /// `d3d11_event_query_complete`。那个 "1ms" 是**假的**：`Sleep`/`nanosleep`
    /// 的精度取决于**进程的系统时钟粒度**，而 Windows 默认为 15.625ms（64Hz），
    /// 且进程必须显式调用 `timeBeginPeriod(1)` 才能提到 ~1ms —— **Erika 全树没有
    /// 任何 `timeBeginPeriod` 调用**（`grep timeBeginPeriod` 为空）。于是每调用一次
    /// `sleep(1ms)`，实际睡满一个完整时钟中断周期 ≈ **15.6ms**。
    ///
    /// 这个栅栏的调用条件是「导入一个新的 D3D11VA 解码帧」——即**每个上屏的视频帧
    /// 恰好一次**，这解释了真机日志的全部形状：
    ///   * `tick avg` 与 `max` 几乎相等且稳定在 15.2~16.4ms（`sleep` 是定长等待，
    ///     真等 GPU 的耗时会散开，不会这么整齐）；
    ///   * 与视频帧率线性相关（9fps→3.92ms、30fps→10.09ms 的拟合）；
    ///   * `Paused` 时降到 0.03ms（暂停不导入新帧，栅栏根本不进）。
    ///
    /// 改成**阻塞式 `GetData`**（`flags = 0`，不带 `D3D11_ASYNC_GETDATA_DONOTFLUSH`）
    /// 后，驱动在 query 完成时直接唤醒本线程，等待精度是 GPU 中断级（亚毫秒），
    /// 且不再有轮询开销。这一改动**不改变同步语义**（仍然等 GPU 真正读完旧纹理
    /// 才把它还给解码器池，不会引入花屏），只是把"定长空等"换成"精确等到就醒"。
    ///
    /// # query 复用（`gpu_fence_query` 槽）
    ///
    /// 本函数**每个上屏视频帧调用一次**（30fps → 每秒 30 次，120Hz 上屏更高）。
    /// 旧实现每次调用都 `CreateQuery` 一个新的 `D3D11_QUERY_EVENT` 对象、返回时
    /// 随局部变量释放，于是每帧都付一次驱动侧对象创建 + runtime 记账。现在改为
    /// 复用同一个 query 对象（计数见 `gpu_fence_query_creates` /
    /// `gpu_fence_query_reuses`）。
    ///
    /// 复用依据（每条都可在 Microsoft Learn 原文核对，此处只引用原文、不引申）：
    ///
    ///   1. **`D3D11_QUERY_EVENT` 只需要 `End`**：`D3D11_QUERY` 枚举文档原文
    ///      "When using this type of query, `ID3D11DeviceContext::Begin` is
    ///      disabled."，所以复用不必（也不能）补 `Begin`。
    ///   2. **同一个 query 对象可以反复 `End`**：`GetData` 文档原文
    ///      "Certain queries only require a call to `End` in which case the data
    ///      returned by `GetData` is accurate up to the last call to `End`"。
    ///      "截至**最后一次** `End`"这一措辞本身就定义了多次 `End` 的语义。
    ///   3. **`Flush()` 必须保留**：命令若还压在命令缓冲里没提交给 GPU，GPU 就
    ///      永远走不到这个 `End` 点。`D3D11_ASYNC_GETDATA_DONOTFLUSH` 文档原文
    ///      给的是同一个机理的反面："Do not flush the command buffer. ...
    ///      Since the commands in the command buffer are not flushed they will not
    ///      be processed and therefore `GetData` will never return S_OK."
    ///      （该文档同时说明不设此标志时 runtime 自己会 flush，所以这里显式
    ///      `Flush()` 可能与 `GetData` 内部的提交重复；原文亦称 "Each call to
    ///      `Flush` incurs a significant amount of overhead."。本函数**保持现状
    ///      不动**：删除它需要真机测量证明 `GetData` 不会长时间挂起，属于待验证项
    ///      而不是已证结论。）
    ///
    /// # 为什么**一个**槽就够（改异步前必须重读）
    ///
    /// 本函数是**同步**的：`End` → `Flush` → **阻塞** `GetData` → 返回。
    /// `GetData` 返回 `S_OK` 意味着 GPU 已经越过这一拍插入的 `End` 标记，因此
    /// 函数返回时槽里的 query 已经**没有在飞的 `End`**；下一次进入本函数时，槽里
    /// 最多只有"上一次已完成、结果已被取走"的 query。同步的调用序列保证任一时刻
    /// **至多一个 `End` 在飞**，所以单个槽足够。
    ///
    /// **若以后改异步**（例如先 `End` 不等结果就继续渲染下一帧、或多帧同时在飞），
    /// 单槽立刻失效：同一个 query 上会同时存在两个未完成的 `End`，而按上面的依据 2，
    /// `GetData` 返回的是"截至**最后一次** `End`"的状态——前一个栅栏的等待条件被
    /// 后一个覆盖，等于**提前把纹理还给解码器**（花屏/撕裂），栅栏也就不再是栅栏。
    /// 那时需要的是 N 个 query 的环形池（N = 允许在飞的最大帧数），而不是这一个槽。
    fn wait_for_gpu(&mut self) -> Result<Duration> {
        let Some(state) = self.state.as_ref() else {
            return Ok(Duration::ZERO);
        };
        // 复用槽：只有槽为空时才新建（`set_device` 换设备时清槽）。
        if self.gpu_fence_query.is_none() {
            let query = create_d3d11_event_query(&state.device)?;
            self.gpu_fence_query = Some(query);
            self.stats.gpu_fence_query_creates += 1;
        } else {
            self.stats.gpu_fence_query_reuses += 1;
        }
        let query = self.gpu_fence_query.as_ref().expect("query ensured");
        unsafe {
            state.context.End(query);
            state.context.Flush();
        }
        let started = Instant::now();
        // 阻塞等待。`D3D11_QUERY_EVENT` 的 `GetData` 在未完成时会挂起调用线程直到
        // GPU 抵达该 `End()` 点；设备被移除（TDR）时返回错误，因此不需要另外套一层
        // 超时循环——旧实现那个 100ms 超时只是 sleep 轮询的补丁。
        let mut complete = BOOL::default();
        let data = unsafe {
            state.context.GetData(
                // 注意必须传引用：`GetData` 要的是 `Param<ID3D11Asynchronous>`，
                // `ID3D11Query` 自身不满足，`&ID3D11Query` 才满足。
                query,
                Some((&mut complete as *mut BOOL).cast::<c_void>()),
                mem::size_of::<BOOL>() as u32,
                0,
            )
        };
        if let Err(error) = data {
            // 失败的 query 状态未知（设备移除/TDR 时 `GetData` 返回
            // `DXGI_ERROR_DEVICE_REMOVED`）。**绝不能留着复用**：下一次 `End` 会落在
            // 一个"已 `End`、结果却没被取走"的 query 上，在依据 2 的语义下那个 `End`
            // 的完成条件等不到 → 栅栏要么立刻误判完成（提前还帧 → 花屏），要么永久挂起。
            self.gpu_fence_query = None;
            return Err(d3d_error(
                "ID3D11DeviceContext::GetData(video retirement, blocking)",
                error,
            ));
        }
        Ok(started.elapsed())
    }

    /// 每 [`FRAME_STATS_SAMPLE_TICKS`] 拍采一次 `IDXGISwapChain::GetFrameStatistics`，
    /// 把「显示器 vblank 速率」与「每 vblank 提交/上屏帧数」写进诊断字段。
    ///
    /// 依据、限制与字段语义见 [`D3d11FrameStatsSampler`]。取不到读数时把
    /// [`D3d11RendererStats::frame_stats_available`] 置 false 并**保留上次读数**
    /// —— 「取不到」不等于「0Hz」，把两者混起来会让诊断反向误导。
    fn sample_present_frame_stats(&mut self) {
        let Some(swapchain) = self
            .surface
            .as_ref()
            .and_then(|surface| surface.swapchain.as_ref())
        else {
            self.present_frame_stats = None;
            self.stats.frame_stats_available = false;
            return;
        };
        let swapchain: IDXGISwapChain = match swapchain.cast() {
            Ok(swapchain) => swapchain,
            Err(_) => {
                self.present_frame_stats = None;
                self.stats.frame_stats_available = false;
                return;
            }
        };
        let mut current = DXGI_FRAME_STATISTICS::default();
        if unsafe { swapchain.GetFrameStatistics(&mut current) }.is_err() {
            self.present_frame_stats = None;
            self.stats.frame_stats_available = false;
            return;
        }
        self.stats.frame_stats_available = true;
        let Some(sampler) = self.present_frame_stats.as_mut() else {
            self.present_frame_stats = Some(D3d11FrameStatsSampler {
                last: current,
                last_at: Instant::now(),
                ticks: 0,
            });
            return;
        };
        sampler.ticks = sampler.ticks.wrapping_add(1);
        if sampler.ticks < FRAME_STATS_SAMPLE_TICKS {
            return;
        }
        sampler.ticks = 0;
        let seconds = sampler.last_at.elapsed().as_secs_f64();
        sampler.last_at = Instant::now();
        // 三个计数都是 `u32` 的启动以来累计值（窗口化下自 swapchain 创建起算），
        // 用 `wrapping_sub` 取窗口增量：绕回时差值仍然正确。
        let refresh_delta = current
            .SyncRefreshCount
            .wrapping_sub(sampler.last.SyncRefreshCount) as f64;
        let presents_delta = current.PresentCount.wrapping_sub(sampler.last.PresentCount) as f64;
        let shown_delta = current
            .PresentRefreshCount
            .wrapping_sub(sampler.last.PresentRefreshCount) as f64;
        sampler.last = current;
        if seconds <= 0.0 || refresh_delta <= 0.0 {
            return;
        }
        let to_milli =
            |value: f64| -> u32 { (value * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32 };
        self.stats.display_refresh_hz_milli = to_milli(refresh_delta / seconds);
        self.stats.presents_per_refresh_milli = to_milli(presents_delta / refresh_delta);
        self.stats.shown_frames_per_refresh_milli = to_milli(shown_delta / refresh_delta);
    }

    fn ensure_default_device(&mut self) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let (device, context) = create_default_device()?;
        self.set_device(device, context)
    }

    fn ensure_device_for_texture(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        let frame_device = unsafe { texture.GetDevice() }
            .map_err(|error| d3d_error("ID3D11Texture2D::GetDevice", error))?;
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.device.as_raw() == frame_device.as_raw())
        {
            return Ok(());
        }
        let context = unsafe { frame_device.GetImmediateContext() }
            .map_err(|error| d3d_error("ID3D11Device::GetImmediateContext", error))?;
        self.retire_current_video()?;
        self.danmaku_atlas_cache = None;
        self.overlay_textures.clear();
        self.set_device(frame_device, context)
    }

    /// 设备是否已失效：`GetDeviceRemovedReason` 返回非 S_OK（设备移除/重置）
    /// 即判定。注意设备正常时该调用返回 S_OK，TDR 后返回失败。
    fn device_removed(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| unsafe { state.device.GetDeviceRemovedReason().is_err() })
    }

    fn set_device(&mut self, device: ID3D11Device, context: ID3D11DeviceContext) -> Result<()> {
        // 缓存的纹理/图集属于旧设备：设备一换就必须全部失效，否则会拿旧设备
        // 的资源去画（TDR 后旧设备已死，表现为黑屏或花屏）。这里收口，
        // `ensure_device_for_texture` / `ensure_default_device` 两条重建路径
        // 都经过本函数。
        self.overlay_textures.clear();
        self.danmaku_atlas_cache = None;
        // 复用槽里的栅栏 query 同样属于旧设备：`ID3D11Query` 由创建它的
        // `ID3D11Device` 拥有，换设备后在新设备上对它 `End`/`GetData` 无效
        // （TDR 后旧设备已死，更是必然失败）。丢弃，不要留着复用。
        self.gpu_fence_query = None;
        // 帧统计计数器按 swapchain 记（窗口化下自创建起算），换设备必然换链，
        // 留着旧窗口会在新链上算出一个纯属虚构的刷新率。
        self.present_frame_stats = None;
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let state = D3d11DeviceState::new(device, context)?;
        if let Err(error) = self
            .upscaler
            .attach_device(&state.device, self.upscaler_mode)
        {
            report_upscaler_failure("pipeline_build", self.upscaler_mode, &error);
        }
        // The existing swap chain belongs to the previous device; ResizeBuffers
        // cannot migrate it. Drop it so recreate_surface_targets creates a new
        // one — COM identity changes, which the host must rebind.
        if let Some(old) = self.state.as_ref() {
            if let Some(surface) = self.surface.as_mut() {
                release_backbuffer_views(&old.context, surface);
                // 设备切换同样要先摘掉 panel 上的旧 swapchain（见
                // panel_clear_swap_chain 注释）：panel 仍持有对旧 swapchain 的
                // 引用，直接 drop 会让合成器悬垂消费。clear 失败也继续——旧设备
                // 已失效，此时摘除是尽力而为。
                if let Some(panel) = surface.panel {
                    let _ = panel_clear_swap_chain(panel);
                }
                surface.swapchain = None;
                surface.output_texture = None;
            }
        }
        // The cached LUT SRV belongs to the previous device, and D3D11 cannot
        // bind a resource across devices. Drop it even when the key is
        // unchanged: the CPU-side `gamut_lut_job` only holds parameters, so a
        // later frame regenerates the texture on the new device and reuses it.
        self.gamut_lut = None;
        self.state = Some(state);
        self.recreate_surface_targets()?;
        Ok(())
    }

    fn recreate_surface_targets(&mut self) -> Result<()> {
        // 换面可能换链（新建）也可能原地 `ResizeBuffers`（同一个链）。两种都
        // 从新窗口重新采样：`GetFrameStatistics` 的计数是"自 swapchain 创建
        // 起算"的累计值，跨链相减会得到负数/虚构值。
        self.present_frame_stats = None;
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        let Some(state) = self.state.as_ref() else {
            return Ok(());
        };
        trace("recreate_surface_targets: reset views");
        release_backbuffer_views(&state.context, surface);
        let output_mode = surface.output_mode;
        let width = surface.metrics.physical_extent.width.max(1);
        let height = surface.metrics.physical_extent.height.max(1);
        let format = output_mode.swapchain_format();
        let hwnd = surface.hwnd;
        let panel = surface.panel;
        let composition = surface.composition;

        if surface.flutter_texture {
            surface.swapchain = None;
            let (texture, render_target) =
                create_flutter_texture_target(&state.device, width, height)?;
            surface.output_texture = Some(texture);
            surface.render_target = Some(render_target);
            surface.output_mode = D3d11OutputMode::Sdr;
            self.stats.surface_width = surface.metrics.physical_extent.width;
            self.stats.surface_height = surface.metrics.physical_extent.height;
            self.stats.hdr10_output_active = false;
            return Ok(());
        }
        surface.output_texture = None;

        // Size and SDR↔HDR format changes keep the same IDXGISwapChain1 so a
        // DirectComposition visual's SetContent stays valid. Device changes
        // drop the chain in set_device before reaching here.
        let reused = if let Some(swapchain) = surface.swapchain.as_ref() {
            trace("recreate_surface_targets: ResizeBuffers");
            unsafe {
                swapchain
                    .ResizeBuffers(0, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
                    .is_ok()
            }
        } else {
            false
        };
        if !reused {
            if surface.swapchain.is_some() {
                trace("recreate_surface_targets: ResizeBuffers failed, creating new swapchain");
            } else {
                trace("recreate_surface_targets: create_swapchain");
            }
            // 摘掉旧 swapchain 再释放（见 panel_clear_swap_chain 注释）：换源/
            // HDR 切换/ResizeBuffers 失败重建都会走到这里，不先摘除就 drop 会与
            // 在飞的异步 present 竞态 → use-after-free。clear 失败也继续释放——
            // 最坏情况是 panel 已失效（无人在消费旧 swapchain），直接释放反而安全。
            if let (Some(panel), true) = (panel, surface.swapchain.is_some()) {
                let _ = panel_clear_swap_chain(panel);
            }
            surface.swapchain = None;
            surface.swapchain = Some(create_swapchain(
                &state.device,
                hwnd,
                panel,
                width,
                height,
                format,
                composition,
                composition && self.video_alpha_mode.has_alpha(),
            )?);
        }
        let swapchain = surface.swapchain.as_ref().expect("swapchain ensured");
        configure_swapchain_color_space(swapchain, output_mode)?;
        if matches!(output_mode, D3d11OutputMode::Sdr) {
            let _ = clear_hdr_metadata(swapchain);
        }
        trace("recreate_surface_targets: create_render_target");
        surface.render_target = Some(create_render_target(&state.device, swapchain)?);
        if matches!(output_mode, D3d11OutputMode::Hdr10) {
            trace("recreate_surface_targets: create_linear_render_target");
            let (render_target, shader_resource) =
                create_linear_render_target(&state.device, width, height)?;
            surface.linear_render_target = Some(render_target);
            surface.linear_shader_resource = Some(shader_resource);
        }
        self.stats.surface_width = surface.metrics.physical_extent.width;
        self.stats.surface_height = surface.metrics.physical_extent.height;
        self.stats.hdr10_output_active = matches!(output_mode, D3d11OutputMode::Hdr10);
        // 尺寸诊断：backbuffer 重建打点（定位"播着播着画面变小"是否因 surface
        // 在某事件（换源/seek/GPU 自愈）后被重建为错误尺寸）。
        eprintln!(
            "[size-diag] surface rebuilt: backbuffer={}x{} mode={:?}",
            surface.metrics.physical_extent.width,
            surface.metrics.physical_extent.height,
            output_mode,
        );
        Ok(())
    }

    fn composition_swapchain(&self) -> Option<&IDXGISwapChain1> {
        let surface = self.surface.as_ref()?;
        if !surface.composition {
            return None;
        }
        surface.swapchain.as_ref()
    }

    fn set_output_mode(&mut self, output_mode: D3d11OutputMode) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            self.stats.hdr10_output_active = false;
            return Ok(());
        };
        let output_mode = if surface.flutter_texture {
            D3d11OutputMode::Sdr
        } else {
            output_mode
        };
        if surface.output_mode == output_mode && surface.targets_ready() {
            self.stats.hdr10_output_active = matches!(output_mode, D3d11OutputMode::Hdr10);
            return Ok(());
        }
        surface.output_mode = output_mode;
        self.retire_current_video()?;
        self.recreate_surface_targets()
    }

    fn select_output_mode_for_source(
        &mut self,
        source: SourceColorState,
    ) -> Result<D3d11OutputMode> {
        let source_is_hdr = source.is_hdr();
        if source_is_hdr {
            self.stats.hdr_source_frames += 1;
        }
        // DirectComposition transparency is defined on the SDR premultiplied
        // swap-chain path. Packed-alpha assets are effects/UI content rather
        // than an HDR presentation plane, so keep their color and alpha in one
        // stable BGRA8 composition space.
        if self.video_alpha_mode.has_alpha()
            || self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.flutter_texture)
        {
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            if source_is_hdr {
                self.stats.sdr_tonemap_frames += 1;
            }
            return Ok(D3d11OutputMode::Sdr);
        }
        if matches!(source.transfer, TransferFunction::Pq)
            && self.try_enable_hdr10_output(source)?
        {
            self.stats.hdr10_output_frames += 1;
            return Ok(D3d11OutputMode::Hdr10);
        }

        self.set_output_mode(D3d11OutputMode::Sdr)?;
        if source_is_hdr {
            self.stats.sdr_tonemap_frames += 1;
        }
        Ok(D3d11OutputMode::Sdr)
    }

    fn try_enable_hdr10_output(&mut self, source: SourceColorState) -> Result<bool> {
        if self.state.is_none() || self.surface.is_none() {
            return Ok(false);
        }
        if self.hdr10_output_unavailable {
            return Ok(false);
        }

        if let Err(error) = self.set_output_mode(D3d11OutputMode::Hdr10) {
            self.stats.hdr10_output_failures += 1;
            self.hdr10_output_unavailable = true;
            trace(&format!("hdr10 output unavailable: {error}"));
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            return Ok(false);
        }
        if let Err(error) = self.update_hdr10_metadata(source) {
            self.stats.hdr10_metadata_failures += 1;
            self.hdr10_output_unavailable = true;
            trace(&format!("hdr10 metadata unavailable: {error}"));
            self.set_output_mode(D3d11OutputMode::Sdr)?;
            return Ok(false);
        }
        Ok(true)
    }

    fn update_hdr10_metadata(&mut self, source: SourceColorState) -> Result<()> {
        let swapchain = self
            .surface
            .as_ref()
            .and_then(|surface| surface.swapchain.as_ref())
            .ok_or_else(|| {
                PlayerError::Renderer("d3d11: no swapchain for HDR10 metadata".into())
            })?;
        set_hdr10_metadata(swapchain, dxgi_hdr10_metadata(source))?;
        self.stats.hdr10_metadata_updates += 1;
        Ok(())
    }

    fn import_d3d11va_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        trace("import_d3d11va_frame: enter");
        // ── 首帧导入耗时拆解（前 3 帧一次性）──
        // 宿主 bili-rs 实测首帧 `pump_video` 阻塞 ~450ms（"点开先黑一下"里占
        // 比不小）。这里逐步计时，定位卡在哪一步：跨设备 OpenSharedResource
        // 冷启动 / SRV 创建 / 退役上一帧。走 stderr（宿主已 SetStdHandle 把
        // stderr 重定向到 %TEMP%\bili-rs-stderr.log）。
        use std::sync::atomic::{AtomicU32, Ordering};
        static IMPORT_PROBE: AtomicU32 = AtomicU32::new(0);
        let probe_n = IMPORT_PROBE.fetch_add(1, Ordering::Relaxed);
        let t_enter = Instant::now();
        let Some(texture_ref) = frame.frame.d3d11va_texture() else {
            return Err(PlayerError::Renderer(
                "d3d11: hardware frame is not a D3D11VA texture".to_string(),
            ));
        };
        let retained_frame = frame
            .frame
            .decoded_frame()
            .ok_or_else(|| {
                PlayerError::Renderer(
                    "d3d11: D3D11VA texture is missing its decoded AVFrame backing".to_string(),
                )
            })?
            .try_clone_ref()
            .map_err(|error| {
                PlayerError::Renderer(format!("d3d11: av_frame_ref failed: {error}"))
            })?;
        let t_ref = t_enter.elapsed();
        let source_texture = clone_d3d11_texture(texture_ref.raw_texture())?;
        let texture = self.import_texture_for_current_device(&source_texture)?;
        let t_import = t_enter.elapsed();

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let texture_format = D3d11VideoTextureFormat::from_dxgi(desc.Format).ok_or_else(|| {
            PlayerError::Renderer(format!(
                "d3d11: unsupported D3D11VA texture format {:?}",
                desc.Format
            ))
        })?;
        let array_index = texture_ref.array_index();
        if array_index >= desc.ArraySize {
            return Err(PlayerError::Renderer(format!(
                "d3d11: D3D11VA array index {array_index} out of bounds for {} slices",
                desc.ArraySize
            )));
        }
        let source_color = source_color_for_frame(frame);
        let output_mode = self.select_output_mode_for_source(source_color)?;
        let target_color = output_mode.target_color_for_source(source_color);
        let visible_width = texture_ref.width().max(1).min(desc.Width.max(1));
        let visible_height = texture_ref.height().max(1).min(desc.Height.max(1));

        let state = self.state.as_ref().expect("device ensured");
        let luma = create_plane_srv(state, &texture, array_index, texture_format.luma_srv())
            .map_err(|error| d3d11va_srv_error(error, &desc, array_index))?;
        let chroma = create_plane_srv(state, &texture, array_index, texture_format.chroma_srv())
            .map_err(|error| d3d11va_srv_error(error, &desc, array_index))?;
        let t_srv = t_enter.elapsed();
        let frame_token = self.next_frame_token;
        self.next_frame_token = self.next_frame_token.wrapping_add(1);
        self.stats.hardware_video_frames += 1;
        self.stats.zero_copy_video_frames += 1;
        self.stats.direct_zero_copy_video_frames += 1;
        let imported = ImportedVideoFrame {
            _frame: Some(retained_frame),
            _texture: texture,
            _chroma_texture: None,
            luma,
            chroma,
            width: visible_width,
            height: visible_height,
            tex_rect: D3d11TexRect::visible_region(
                visible_width,
                visible_height,
                desc.Width,
                desc.Height,
            ),
            _array_index: array_index,
            frame_token,
            constants: constants_for_frame(source_color, texture_format, target_color)
                .packed_alpha_right(self.video_alpha_mode.has_alpha()),
        };
        self.retire_current_video()?;
        // 只打「慢导入」（>5ms）：稳态每帧才几十 µs，全量打会把日志淹掉。
        // 目的：判断 395ms 的跨设备 `OpenSharedResource` 冷启动是**进程一次**、
        // 还是**每开一个新播放窗（=新建渲染设备）就重来**——后者意味着"复用
        // 播放窗"能白赚 ~400ms。宿主把 stderr 重定向到
        // %TEMP%\bili-rs-stderr.log，复盘按 `[erika-import]` 过滤。
        {
            let t_total = t_enter.elapsed();
            if t_total > Duration::from_millis(5) {
                eprintln!(
                    "[erika-import] frame#{probe_n}: ref={:?} OpenSharedResource={:?} srv={:?} retire={:?} total={:?}",
                    t_ref,
                    t_import - t_ref,
                    t_srv - t_import,
                    t_total - t_srv,
                    t_total
                );
            }
        }
        self.current_video = Some(imported);
        Ok(())
    }

    fn upload_software_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        let planar = frame
            .frame
            .decoded_frame()
            .and_then(Frame::to_planar_frame)
            .ok_or_else(|| {
                PlayerError::Renderer(format!(
                    "d3d11: unsupported software video frame format {:?}",
                    frame.frame.pixel_format()
                ))
            })?;
        self.upload_planar_frame(planar, source_color_for_frame(frame))
    }

    fn upload_planar_frame(
        &mut self,
        planar: PlanarFrame,
        source_color: SourceColorState,
    ) -> Result<()> {
        self.ensure_default_device()?;
        let output_mode = self.select_output_mode_for_source(source_color)?;
        let target_color = output_mode.target_color_for_source(source_color);
        let (texture_format, bytes_per_sample) = match planar.format {
            PlanarPixelFormat::Nv12 => (D3d11VideoTextureFormat::Nv12, 1),
            PlanarPixelFormat::P010 => (D3d11VideoTextureFormat::P010, 2),
        };
        let state = self.state.as_ref().expect("device ensured");
        // Separate planes support odd visible dimensions and retain 10-bit HDR
        // samples without requiring hardware video-format texture support.
        let luma_pitch = planar
            .width
            .checked_mul(bytes_per_sample)
            .ok_or_else(|| PlayerError::Renderer("d3d11: luma row pitch overflow".into()))?;
        let chroma_pitch = planar
            .width
            .div_ceil(2)
            .checked_mul(2 * bytes_per_sample)
            .ok_or_else(|| PlayerError::Renderer("d3d11: chroma row pitch overflow".into()))?;
        let luma = create_cpu_texture(
            state,
            planar.width,
            planar.height,
            texture_format.luma_srv(),
            &planar.luma,
            luma_pitch,
        )?;
        let chroma = create_cpu_texture(
            state,
            planar.width.div_ceil(2),
            planar.height.div_ceil(2),
            texture_format.chroma_srv(),
            &planar.chroma,
            chroma_pitch,
        )?;
        let frame_token = self.next_frame_token;
        self.retire_current_video()?;
        self.current_video = Some(ImportedVideoFrame {
            _frame: None,
            _texture: luma._texture,
            _chroma_texture: Some(chroma._texture),
            luma: luma.view,
            // Each SRV retains its underlying texture after this upload returns.
            chroma: chroma.view,
            width: planar.width,
            height: planar.height,
            tex_rect: D3d11TexRect::FULL,
            _array_index: 0,
            frame_token,
            constants: constants_for_frame(source_color, texture_format, target_color)
                .packed_alpha_right(self.video_alpha_mode.has_alpha()),
        });
        self.next_frame_token = self.next_frame_token.wrapping_add(1);
        self.stats.software_video_frames += 1;
        self.stats.cpu_video_frame_fallbacks += 1;
        Ok(())
    }

    fn import_texture_for_current_device(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11Texture2D> {
        if let Some(state) = self.state.as_ref() {
            let frame_device = unsafe { texture.GetDevice() }
                .map_err(|error| d3d_error("ID3D11Texture2D::GetDevice", error))?;
            if state.device.as_raw() == frame_device.as_raw() {
                return Ok(texture.clone());
            }
            // 解码器（ffmpeg d3d11va）与渲染器是不同 D3D11 设备。原先走
            // OpenSharedResource 跨设备读取 ffmpeg 帧池纹理——SHARED 纹理
            // 跨设备访问无自动 GPU 同步，解码线程写 + 渲染线程读的并发会
            // 触发 GPU TDR/设备移除（"GPU 设备实例已经暂停"），并产生花屏
            // （读到写一半的色度平面 → 粉屏）。
            // 改为切到解码器设备：ffmpeg 的 d3d11va 设备默认开启
            // multithread protected，解码/渲染多线程并发提交由 D3D11
            // runtime 串行化，安全。走 DirectDecoderDevice 同设备路径，
            // 消除跨设备共享。
            trace(&format!(
                "switching renderer to the decoder D3D11 device to avoid unsynchronized \
                 cross-device surface reuse (renderer {:p} -> decoder {:p})",
                state.device.as_raw(),
                frame_device.as_raw(),
            ));
        }

        self.ensure_device_for_texture(texture)?;
        Ok(texture.clone())
    }

    /// Target color state overlays are composited into: the attached surface's
    /// swapchain encoding (PQ for HDR10, sRGB otherwise).
    fn overlay_target_color(&self) -> TargetColorState {
        self.surface
            .as_ref()
            .map(|surface| surface.output_mode)
            .unwrap_or_default()
            .target_color()
    }

    fn prepare_overlay_draws(
        &mut self,
        frame: Option<&OverlayFrame>,
    ) -> Result<Vec<D3d11OverlayDraw>> {
        let Some(frame) = frame else {
            return Ok(Vec::new());
        };
        if frame.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_default_device()?;
        self.stats.prepared_overlay_frames += 1;
        self.stats.prepared_overlay_subtitle_planes += frame
            .subtitle_planes
            .len()
            .saturating_add(frame.subtitle_alpha_planes.len())
            as u64;
        let viewport_w = frame.viewport.width;
        let viewport_h = frame.viewport.height;
        let target = self.overlay_target_color();
        let mut draws = Vec::new();

        for plane in &frame.subtitle_planes {
            if plane.width == 0 || plane.height == 0 {
                continue;
            }
            let expected = plane.width as usize * plane.height as usize * 4;
            if plane.rgba.len() != expected {
                return Err(PlayerError::Renderer(format!(
                    "d3d11: overlay subtitle plane has {} bytes, expected {expected} for {}x{} RGBA",
                    plane.rgba.len(),
                    plane.width,
                    plane.height
                )));
            }
            let texture = {
                let state = self.state.as_ref().expect("device ensured");
                self.overlay_textures.acquire(
                    state,
                    plane.width,
                    plane.height,
                    DXGI_FORMAT_R8G8B8A8_UNORM,
                    &plane.rgba,
                    plane.width * 4,
                    &mut self.stats,
                )?
            };
            let (x, y, width, height) = plane.scaled_rect(viewport_w, viewport_h);
            draws.push(D3d11OverlayDraw {
                texture,
                constants: OverlayUniforms::rgba_plane(
                    x, y, width, height, viewport_w, viewport_h, target,
                ),
            });
        }

        self.append_alpha_atlas_draws(frame, viewport_w, viewport_h, target, &mut draws)?;
        Ok(draws)
    }

    fn prepare_danmaku_draws(
        &mut self,
        plan: Option<&DanmakuRenderPlan>,
    ) -> Result<Vec<D3d11OverlayDraw>> {
        let Some(plan) = plan else {
            return Ok(Vec::new());
        };
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_default_device()?;
        let Some(atlas) = plan.atlas.as_ref() else {
            return Ok(Vec::new());
        };
        if !atlas.is_valid() {
            return Err(PlayerError::Renderer(format!(
                "d3d11: danmaku glyph atlas has fill={} outline={} bytes, expected at least {} for {}x{} stride {}",
                atlas.fill_alpha.len(),
                atlas.outline_alpha.len(),
                atlas.required_len(),
                atlas.width,
                atlas.height,
                atlas.stride
            )));
        }
        let viewport_w = plan.viewport.width;
        let viewport_h = plan.viewport.height;
        let target = self.overlay_target_color();
        let mut draws = Vec::with_capacity(plan.items.len() * 3);
        let (fill, outline) = self.prepare_danmaku_atlas_textures(atlas)?;
        for item in &plan.items {
            self.append_danmaku_glyph_draws(
                item, &fill, &outline, viewport_w, viewport_h, target, &mut draws,
            );
        }
        Ok(draws)
    }

    fn prepare_danmaku_atlas_textures(
        &mut self,
        atlas: &DanmakuGlyphAtlas,
    ) -> Result<(D3d11OverlayTexture, D3d11OverlayTexture)> {
        if let Some(cache) = &self.danmaku_atlas_cache {
            if cache.can_reuse_for(atlas) {
                self.stats.overlay_alpha_atlas_reuses += 1;
                return Ok((cache.fill.clone(), cache.outline.clone()));
            }
        }
        let incremental = self.danmaku_atlas_cache.as_ref().and_then(|cache| {
            atlas
                .incremental_update_from(cache.version, cache.width, cache.height, cache.stride)
                .map(|update| (cache.fill.clone(), cache.outline.clone(), update.clone()))
        });
        if let Some((fill, outline, update)) = incremental {
            self.update_danmaku_atlas_texture(&fill, atlas, &atlas.fill_alpha, &update);
            self.update_danmaku_atlas_texture(&outline, atlas, &atlas.outline_alpha, &update);
            if let Some(cache) = &mut self.danmaku_atlas_cache {
                cache.version = atlas.version;
            }
            self.stats.overlay_alpha_atlas_uploads += 1;
            self.stats.overlay_alpha_atlas_reuses += 1;
            return Ok((fill, outline));
        }

        let (fill, outline) = {
            let state = self.state.as_ref().expect("device ensured");
            (
                create_cpu_texture(
                    state,
                    atlas.width,
                    atlas.height,
                    DXGI_FORMAT_R8_UNORM,
                    &atlas.fill_alpha,
                    atlas.stride as u32,
                )?,
                create_cpu_texture(
                    state,
                    atlas.width,
                    atlas.height,
                    DXGI_FORMAT_R8_UNORM,
                    &atlas.outline_alpha,
                    atlas.stride as u32,
                )?,
            )
        };
        self.stats.overlay_alpha_atlas_uploads += 1;
        self.danmaku_atlas_cache = Some(D3d11DanmakuAtlasCache {
            version: atlas.version,
            width: atlas.width,
            height: atlas.height,
            stride: atlas.stride,
            fill: fill.clone(),
            outline: outline.clone(),
        });
        Ok((fill, outline))
    }

    fn update_danmaku_atlas_texture(
        &self,
        texture: &D3d11OverlayTexture,
        atlas: &DanmakuGlyphAtlas,
        pixels: &[u8],
        update: &DanmakuAtlasUpdate,
    ) {
        let offset = update.y as usize * atlas.stride + update.x as usize;
        let update_box = D3D11_BOX {
            left: update.x,
            top: update.y,
            front: 0,
            right: update.x.saturating_add(update.width),
            bottom: update.y.saturating_add(update.height),
            back: 1,
        };
        unsafe {
            self.state
                .as_ref()
                .expect("device ensured")
                .context
                .UpdateSubresource(
                    &texture._texture,
                    0,
                    Some(&update_box as *const D3D11_BOX),
                    pixels[offset..].as_ptr().cast::<c_void>(),
                    atlas.stride as u32,
                    0,
                );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_danmaku_glyph_draws(
        &self,
        item: &DanmakuGlyphInstance,
        fill_texture: &D3d11OverlayTexture,
        outline_texture: &D3d11OverlayTexture,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
        draws: &mut Vec<D3d11OverlayDraw>,
    ) {
        if item.shadow_rgba[3] > 0.0 {
            let mut rect = item.rect;
            rect[0] += item.shadow_offset[0];
            rect[1] += item.shadow_offset[1];
            draws.push(D3d11OverlayDraw {
                texture: outline_texture.clone(),
                constants: OverlayUniforms::alpha_atlas_rect(
                    item.shadow_rgba,
                    rect,
                    item.tex_rect,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        if item.outline_rgba[3] > 0.0 {
            draws.push(D3d11OverlayDraw {
                texture: outline_texture.clone(),
                constants: OverlayUniforms::alpha_atlas_rect(
                    item.outline_rgba,
                    item.rect,
                    item.tex_rect,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        draws.push(D3d11OverlayDraw {
            texture: fill_texture.clone(),
            constants: OverlayUniforms::alpha_atlas_rect(
                item.color_rgba,
                item.rect,
                item.tex_rect,
                viewport_w,
                viewport_h,
                target,
            ),
        });
    }

    /// 把本拍的 alpha 位图横向拼成一张 `DXGI_FORMAT_R8_UNORM` 图集，按平面排进
    /// `draws`。拼接缓冲挂在 `self.alpha_atlas_scratch` 上跨帧复用
    /// （改动前每拍新建，见 [`D3d11AlphaAtlasScratch`]）。
    fn append_alpha_atlas_draws(
        &mut self,
        frame: &OverlayFrame,
        viewport_w: u32,
        viewport_h: u32,
        target: TargetColorState,
        draws: &mut Vec<D3d11OverlayDraw>,
    ) -> Result<()> {
        let bitmaps = &frame.subtitle_alpha_planes;
        if !self.alpha_atlas_scratch.rebuild(bitmaps)? {
            return Ok(());
        }
        let atlas_width = self.alpha_atlas_scratch.width;
        let atlas_height = self.alpha_atlas_scratch.height;

        let texture = {
            let state = self.state.as_ref().expect("device ensured");
            self.overlay_textures.acquire(
                state,
                atlas_width as u32,
                atlas_height as u32,
                DXGI_FORMAT_R8_UNORM,
                &self.alpha_atlas_scratch.pixels[..atlas_width * atlas_height],
                atlas_width as u32,
                &mut self.stats,
            )?
        };
        self.stats.overlay_alpha_atlas_uploads += 1;
        for &(index, atlas_x) in &self.alpha_atlas_scratch.placements {
            let bitmap = &bitmaps[index];
            draws.push(D3d11OverlayDraw {
                texture: texture.clone(),
                constants: OverlayUniforms::alpha_atlas(
                    bitmap.color_rgba,
                    bitmap.placement.x,
                    bitmap.placement.y,
                    bitmap.placement.width,
                    bitmap.placement.height,
                    atlas_x as u32,
                    atlas_width as u32,
                    atlas_height as u32,
                    viewport_w,
                    viewport_h,
                    target,
                ),
            });
        }
        Ok(())
    }

    /// Return a cached (or freshly generated) perceptual gamut LUT SRV for
    /// the given uniforms, or `None` while the fast path is active or the
    /// background generation is still pending. Callers must then mask
    /// `gamut_lut_enabled` off so the shader keeps the fast path.
    fn gamut_lut_view(
        &mut self,
        uniforms: &VideoUniforms,
    ) -> Result<Option<ID3D11ShaderResourceView>> {
        if uniforms.gamut_lut_enabled == 0 {
            return Ok(None);
        }
        let packed = uniforms._gamut_primaries;
        let source = packed >> 8;
        let target = packed & 0xff;
        // The LUT's I axis spans [target black, target peak] in PQ codes;
        // either endpoint changing invalidates the cached LUT.
        let black_pq = d3d_quantize_luma_pq(uniforms.tone_map_extra[2]);
        let peak_pq = d3d_quantize_luma_pq(uniforms.nits[1]);
        if let Some((s, t, b, p, srv)) = &self.gamut_lut {
            if *s == source && *t == target && *b == black_pq && *p == peak_pq {
                return Ok(Some(srv.clone()));
            }
        }
        let params = GamutLutParams {
            source: d3d_code_to_primaries(source),
            target: d3d_code_to_primaries(target),
            // Same target black the shader derives from tone_map_extra.z.
            min_luma: d3d_pq_code_for_lut(uniforms.tone_map_extra[2]),
            max_luma: d3d_pq_code_for_lut(uniforms.nits[1]),
        };
        let job_params = self
            .gamut_lut_job
            .as_ref()
            .map(GamutLutJob::params)
            .filter(|job_params| *job_params == params);
        if job_params.is_none() {
            // First request (or the key changed): spawn generation and keep
            // the fast path for this frame.
            self.gamut_lut_job = Some(GamutLutJob::spawn(params));
            return Ok(None);
        }
        let Some(lut) = self.gamut_lut_job.as_ref().and_then(GamutLutJob::poll) else {
            return Ok(None);
        };
        self.gamut_lut_job = None;
        let state = self.state.as_ref().expect("device ensured for gamut lut");
        // Pack (I, P+0.5, T+0.5) into RGBA16F bytes laid out I x C x H.
        let texels = pack_rgba16f(&lut.texels, 1.0);
        let initial = [D3D11_SUBRESOURCE_DATA {
            pSysMem: texels.as_ptr() as *const c_void,
            SysMemPitch: (LUT_SIZE_I * 4 * 2) as u32,
            SysMemSlicePitch: (LUT_SIZE_I * LUT_SIZE_C * 4 * 2) as u32,
        }];
        let desc = D3D11_TEXTURE3D_DESC {
            Width: LUT_SIZE_I as u32,
            Height: LUT_SIZE_C as u32,
            Depth: LUT_SIZE_H as u32,
            MipLevels: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            state
                .device
                .CreateTexture3D(&desc, Some(initial.as_ptr()), Some(&mut texture))
                .map_err(|error| d3d_error("ID3D11Device::CreateTexture3D(gamut lut)", error))?;
        }
        let resource = texture
            .expect("gamut lut texture created")
            .cast::<ID3D11Resource>()
            .map_err(|error| d3d_error("cast to ID3D11Resource", error))?;
        let mut srv = None;
        unsafe {
            state
                .device
                .CreateShaderResourceView(&resource, None, Some(&mut srv))
                .map_err(|error| {
                    d3d_error("ID3D11Device::CreateShaderResourceView(gamut lut)", error)
                })?;
        }
        let srv = srv.expect("gamut lut srv created");
        self.gamut_lut = Some((source, target, black_pq, peak_pq, srv.clone()));
        Ok(Some(srv))
    }

    fn render_video(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        trace("render_video: enter");
        if self.current_video.is_none() {
            return Ok(false);
        }
        self.ensure_default_device()?;
        self.ensure_surface_ready()?;
        let flutter_texture = self
            .surface
            .as_ref()
            .expect("surface ensured")
            .flutter_texture;
        let frame_token = self
            .current_video
            .as_ref()
            .expect("video checked")
            .frame_token;
        if flutter_texture
            && self.surface.as_ref().unwrap().published_texture.is_some()
            && self
                .flutter_scene
                .as_ref()
                .is_some_and(|scene| scene.matches(frame_token, self.upscaler_mode, context))
        {
            return Ok(true);
        }
        // 本帧序号 +1：叠加层纹理缓存据此判定「这条尺寸本帧是否已经交出去」，
        // 同帧内两张同尺寸平面必须各占一张纹理。
        self.overlay_textures.begin_frame();
        let overlay_prepare_started = Instant::now();
        let overlay_draws = self.prepare_overlay_draws(context.overlay)?;
        let overlay_prepare_duration = overlay_prepare_started.elapsed();
        let danmaku_prepare_started = Instant::now();
        let danmaku_draws = self.prepare_danmaku_draws(context.danmaku)?;
        let danmaku_prepare_duration = danmaku_prepare_started.elapsed();
        let (video_width, video_height, frame_token, native_luma) = {
            let video = self.current_video.as_ref().expect("video checked");
            (
                video.width,
                video.height,
                video.frame_token,
                video.luma.clone(),
            )
        };
        let physical = self
            .surface
            .as_ref()
            .expect("surface ensured")
            .metrics
            .physical_extent;
        let logical_video_width = self.video_alpha_mode.logical_width(video_width);
        let target_rect = aspect_fit_rect(
            logical_video_width,
            video_height,
            physical.width,
            physical.height,
        );
        // 尺寸诊断（变化节流）：对比「视频画面实际占用」与「播放区域
        // （backbuffer）」。fill≈1.0 表示占满；<1.0 表示画面缩小（等比例
        // 缩小 → 从左上角出现 L 型空白）。配合 surface rebuilt 打点定位触发源。
        let diag_key = (
            video_width,
            video_height,
            physical.width,
            physical.height,
            target_rect.x as u32,
            target_rect.y as u32,
            target_rect.width as u32,
            target_rect.height as u32,
        );
        if self.last_size_diag != Some(diag_key) {
            self.last_size_diag = Some(diag_key);
            let fill_w = if physical.width > 0 {
                target_rect.width as f64 / physical.width as f64
            } else {
                0.0
            };
            let fill_h = if physical.height > 0 {
                target_rect.height as f64 / physical.height as f64
            } else {
                0.0
            };
            eprintln!(
                "[size-diag] video={video_width}x{video_height} logical_w={logical_video_width} backbuffer={}x{} target_rect=({:.0},{:.0} {:.0}x{:.0}) fill={fill_w:.3}x{fill_h:.3}",
                physical.width,
                physical.height,
                target_rect.x,
                target_rect.y,
                target_rect.width,
                target_rect.height,
            );
        }
        let upscale_requested = !self.video_alpha_mode.has_alpha()
            && self.upscaler_mode.is_enabled()
            && self.upscaler.status() == LumaUpscalerBackendStatus::Scalar
            && target_rect.width > video_width as f32;
        let upscaled_luma = if upscale_requested {
            let (device, device_context) = {
                let state = self.state.as_ref().expect("device ensured");
                (state.device.clone(), state.context.clone())
            };
            match self.upscaler.encode(
                &device,
                &device_context,
                &native_luma,
                video_width,
                video_height,
                Some(frame_token),
            ) {
                Ok(output) => output,
                Err(error) => {
                    self.upscaler.record_runtime_failure(&error);
                    report_upscaler_failure("frame_encode", self.upscaler_mode, &error);
                    None
                }
            }
        } else {
            None
        };
        let mut video_constants = {
            let video = self.current_video.as_ref().expect("video checked");
            video.constants
        };
        let gamut_lut = self.gamut_lut_view(&video_constants)?;
        if gamut_lut.is_none() {
            // Background generation pending: keep the fast gamut_compress
            // path until the LUT lands.
            video_constants.gamut_lut_enabled = 0;
        }
        let video = self.current_video.as_ref().expect("video checked");
        let state = self.state.as_ref().expect("device ensured");
        let surface = self.surface.as_ref().expect("surface ensured");
        let rtv = surface
            .render_target
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("d3d11: no render target attached".to_string()))?;
        let linear_target = if matches!(surface.output_mode, D3d11OutputMode::Hdr10) {
            Some((
                surface.linear_render_target.as_ref().ok_or_else(|| {
                    PlayerError::Renderer("d3d11: no linear render target attached".to_string())
                })?,
                surface.linear_shader_resource.as_ref().ok_or_else(|| {
                    PlayerError::Renderer("d3d11: no linear shader resource attached".to_string())
                })?,
            ))
        } else {
            None
        };
        let scene_rtv = linear_target.map_or(rtv, |(linear_rtv, _)| linear_rtv);
        unsafe {
            state.context.ClearRenderTargetView(
                scene_rtv,
                &[
                    0.0,
                    0.0,
                    0.0,
                    if self.video_alpha_mode.has_alpha() {
                        0.0
                    } else {
                        1.0
                    },
                ],
            );
        }
        let draw_video_started = Instant::now();
        state.draw_video(
            video,
            video_constants,
            upscaled_luma.as_ref(),
            gamut_lut.as_ref(),
            scene_rtv,
            target_rect,
        )?;
        let draw_video_duration = draw_video_started.elapsed();
        let draw_overlay_started = Instant::now();
        if !overlay_draws.is_empty() {
            // Subtitle coordinates are produced in the video-frame viewport.
            // Composite them through the same aspect-fit viewport as the video
            // so resizing the window cannot stretch the subtitle independently.
            state.draw_overlays(&overlay_draws, scene_rtv, target_rect)?;
        }
        if !danmaku_draws.is_empty() {
            state.draw_overlays(
                &danmaku_draws,
                scene_rtv,
                D3d11DrawRect {
                    x: 0.0,
                    y: 0.0,
                    width: physical.width.max(1) as f32,
                    height: physical.height.max(1) as f32,
                },
            )?;
        }
        if let Some((_, linear_srv)) = linear_target {
            state.draw_encode(
                linear_srv,
                rtv,
                physical.width,
                physical.height,
                video.constants,
            )?;
        }
        let draw_overlay_duration = draw_overlay_started.elapsed();
        // present 单独计时：这是「UI 线程是否被同步 present 卡住」的唯一直接证据。
        // `Present1(0, DXGI_PRESENT_DO_NOT_WAIT)` 在队列满时**立即**返回
        // `DXGI_ERROR_WAS_STILL_DRAWING`，所以正常值应当是亚毫秒；真机上若看到
        // 它稳定在 10ms+，那才是 vsync 同步等待（此时看 present_submitted /
        // present_dropped 的比值即可分辨「真的等」还是「一直丢」）。
        let present_started = Instant::now();
        let mut present_outcome = PresentOutcome::Dropped;
        if let Some(swapchain) = surface.swapchain.as_ref() {
            present_outcome = present_swapchain(swapchain, "IDXGISwapChain1::Present1")?;
        }
        let present_duration = present_started.elapsed();
        if flutter_texture {
            self.publish_flutter_texture()?;
            if let Some(scene) = self.flutter_scene.as_mut() {
                scene.update(frame_token, self.upscaler_mode, context);
            } else {
                self.flutter_scene = Some(FlutterTextureScene::new(
                    frame_token,
                    self.upscaler_mode,
                    context,
                ));
            }
        }
        self.stats.rendered_frames += 1;
        // —— 分段计时落账（本拍）——
        // 必须在上面所有对 `state`/`surface`/`video` 的不可变借用结束之后写，
        // 否则借用冲突。这些字段由宿主 `runtime_snapshot()` 读出并落到
        // `[perf][CLIENT][1s]` 的「渲染分解」行。
        self.stats.last_overlay_prepare_duration = overlay_prepare_duration;
        self.stats.last_danmaku_prepare_duration = danmaku_prepare_duration;
        self.stats.last_draw_video_duration = draw_video_duration;
        self.stats.last_draw_overlay_duration = draw_overlay_duration;
        self.stats.last_present_duration = present_duration;
        // present 的**真实分布**（不只是一拍）：见 `present_over_8ms` 的文档。
        self.stats.present_calls += 1;
        self.stats.present_total = self.stats.present_total.saturating_add(present_duration);
        if present_duration > self.stats.present_max {
            self.stats.present_max = present_duration;
        }
        if present_duration > PRESENT_SLOW_THRESHOLD {
            self.stats.present_over_1ms += 1;
        }
        if present_duration > PRESENT_COMPOSITOR_WAIT_THRESHOLD {
            self.stats.present_over_8ms += 1;
        }
        self.sample_present_frame_stats();
        match present_outcome {
            PresentOutcome::Submitted => self.stats.present_submitted += 1,
            PresentOutcome::Dropped => self.stats.present_dropped += 1,
        }
        if !danmaku_draws.is_empty() {
            self.stats.danmaku_passes += 1;
            self.stats.danmaku_items += danmaku_draws.len() as u64;
        }
        Ok(true)
    }

    fn ensure_surface_ready(&mut self) -> Result<()> {
        if self.surface.is_none() {
            return Err(PlayerError::Renderer(
                "d3d11: no output surface attached".to_string(),
            ));
        }
        if self
            .surface
            .as_ref()
            .is_some_and(|surface| !surface.targets_ready())
        {
            self.recreate_surface_targets()?;
        }
        Ok(())
    }

    fn render_clear(&mut self, time_seconds: f64) -> Result<()> {
        trace("render_clear: ensure_default_device");
        self.ensure_default_device()?;
        trace("render_clear: ensure_surface_ready");
        self.ensure_surface_ready()?;
        if self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.flutter_texture && surface.published_texture.is_some())
            && self.flutter_scene.is_none()
        {
            return Ok(());
        }
        let state = self.state.as_ref().expect("device ensured");
        let surface = self.surface.as_ref().expect("surface ensured");
        let rtv = surface
            .render_target
            .as_ref()
            .ok_or_else(|| PlayerError::Renderer("d3d11: no render target attached".to_string()))?;
        let _ = time_seconds;
        let color = [
            0.0,
            0.0,
            0.0,
            if self.video_alpha_mode.has_alpha() {
                0.0
            } else {
                1.0
            },
        ];
        // 清屏路径也走同一个 present 计数：它出现在换源/暂停/resize 后。若在
        // 诊断里看到 `present_submitted` **只在清屏时增长**，说明常规播放路径
        // 一直在 `Dropped`（DXGI 队列满）——那正是 UI 线程还能服务窗口消息的
        // 前提，见 `present_swapchain` 的 `Dropped` 文档。
        //
        // 结果先落局部变量再写 `self.stats`：上面 `state`/`surface` 是不可变
        // 借用，在它们存活期间不能改 `self.stats`。
        let clear_present_started = Instant::now();
        let mut clear_outcome = PresentOutcome::Dropped;
        unsafe {
            trace("render_clear: clear");
            state.context.ClearRenderTargetView(rtv, &color);
            trace("render_clear: present");
            if let Some(swapchain) = surface.swapchain.as_ref() {
                clear_outcome = present_swapchain(swapchain, "IDXGISwapChain1::Present1(clear)")?;
            }
        }
        let clear_present_duration = clear_present_started.elapsed();
        self.publish_flutter_texture()?;
        self.flutter_scene = None;
        trace("render_clear: done");
        self.stats.rendered_frames += 1;
        self.stats.last_present_duration = clear_present_duration;
        match clear_outcome {
            PresentOutcome::Submitted => self.stats.present_submitted += 1,
            PresentOutcome::Dropped => self.stats.present_dropped += 1,
        }
        Ok(())
    }
}

impl RendererBackend for D3d11Renderer {
    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        let (hwnd, panel, composition, flutter_texture, metrics) = match surface {
            PlatformSurface::Wgpu(handle) => {
                let panel = match handle.kind {
                    WgpuSurfaceKind::WindowsHwnd => None,
                    WgpuSurfaceKind::WindowsSwapChainPanel => {
                        if handle.raw_window == 0 {
                            return Err(PlayerError::Renderer(
                                "d3d11: WinUI SwapChainPanel pointer is null".to_string(),
                            ));
                        }
                        Some(handle.raw_window as *mut c_void)
                    }
                    _ => {
                        return Err(PlayerError::Renderer(format!(
                            "d3d11: surface kind {:?} is not supported",
                            handle.kind
                        )));
                    }
                };
                let composition = handle.output_capabilities.direct_composition;
                if panel.is_none() && !composition && handle.raw_window == 0 {
                    return Err(PlayerError::Renderer(
                        "d3d11: Windows HWND surface handle is null".to_string(),
                    ));
                }
                (
                    HWND(handle.raw_window as *mut c_void),
                    panel,
                    composition,
                    false,
                    handle.metrics,
                )
            }
            PlatformSurface::FlutterTexture(handle)
                if handle.kind == FlutterTextureKind::WindowsTextureRegistrar =>
            {
                (HWND::default(), None, false, true, handle.metrics)
            }
            PlatformSurface::FlutterTexture(handle) => {
                return Err(PlayerError::Renderer(format!(
                    "d3d11: Flutter texture kind {:?} is not supported",
                    handle.kind
                )));
            }
            PlatformSurface::Metal(_) => {
                return Err(PlayerError::Renderer(
                    "d3d11: Metal surfaces are not supported".to_string(),
                ));
            }
        };
        // 新面板 = 新 swapchain（下面 `swapchain: None`），帧统计窗口必须重来。
        self.present_frame_stats = None;
        self.surface = Some(AttachedSurface {
            hwnd,
            panel,
            composition,
            flutter_texture,
            metrics,
            output_mode: D3d11OutputMode::Sdr,
            swapchain: None,
            output_texture: None,
            published_texture: None,
            render_target: None,
            linear_render_target: None,
            linear_shader_resource: None,
        });
        self.hdr10_output_unavailable = false;
        self.device_lost = false;
        self.stats.attached = true;
        self.recreate_surface_targets()?;
        self.render_clear(0.0)
    }

    fn detach_surface(&mut self) -> Result<()> {
        // 摘掉 swapchain 再释放 surface（见 panel_clear_swap_chain 注释）：直接
        // drop surface（含 swapchain）会让 panel 悬垂引用旧 swapchain。clear 失败
        // 也继续释放——detach 是收尾清理，不能因 panel 已失效而中止。
        let panel = self.surface.as_ref().and_then(|s| s.panel);
        if let Some(panel) = panel {
            let _ = panel_clear_swap_chain(panel);
        }
        self.retire_current_video()?;
        self.surface = None;
        self.danmaku_atlas_cache = None;
        self.overlay_textures.clear();
        self.stats.attached = false;
        self.stats.surface_width = 0;
        self.stats.surface_height = 0;
        Ok(())
    }

    fn resize_surface(&mut self, metrics: SurfaceMetrics) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Err(PlayerError::Renderer(
                "d3d11: no output surface attached".to_string(),
            ));
        };
        if surface.metrics.physical_extent == metrics.physical_extent {
            surface.metrics = metrics;
            return Ok(());
        }
        surface.metrics = metrics;
        self.hdr10_output_unavailable = false;
        let flutter_texture = surface.flutter_texture;
        self.recreate_surface_targets()?;
        if flutter_texture && self.current_video.is_none() {
            self.render_clear(0.0)?;
        }
        Ok(())
    }

    fn render_test_frame(&mut self, time_seconds: f64) -> Result<()> {
        // PQ code value zero is black, so the current clear-only test frame
        // can safely bypass the scene-linear intermediate target.
        self.render_clear(time_seconds)
    }

    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        if frame.frame.d3d11va_texture().is_some() {
            return self.import_d3d11va_frame(frame);
        }
        if frame.frame.has_hw_frames_context() {
            self.stats.hardware_video_frames += 1;
            return Err(PlayerError::Renderer(
                "d3d11: hardware frame is not importable as D3D11VA".to_string(),
            ));
        }
        self.upload_software_frame(frame)
    }

    fn clear_current_frame(&mut self) -> Result<()> {
        self.retire_current_video()?;
        self.danmaku_atlas_cache = None;
        self.overlay_textures.clear();
        if self.surface.is_some() {
            self.render_clear(0.0)?;
        }
        Ok(())
    }

    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        Ok(())
    }

    fn render_current_frame(&mut self, context: RenderFrameContext<'_>) -> Result<bool> {
        let result = self.render_video(context);
        // 无帧可渲染（Ok(false)，current_video 为空）时同样检查设备是否已失效：
        // import 切设备失败会清空 current_video，且错误被 pump_video 吞掉（仅
        // eprintln 记录、不触发自愈）；若此时设备已被 TDR 移除，render_video
        // 空转返回 Ok(false)，走不到下方 Err 分支的 device_lost 检测——在此
        // 补检，否则 presenter 永不触发自愈，画面永久黑屏。设备正常时短路
        // 不调用 GetDeviceRemovedReason，无额外开销。
        let frame_absent = matches!(&result, Ok(false));
        if (result.is_err() || frame_absent) && self.device_removed() {
            // GPU 设备移除/重置（TDR 通常返回 DXGI_ERROR_DEVICE_RESET，硬件
            // 错误返回 DEVICE_REMOVED）后所有设备操作失败。用
            // GetDeviceRemovedReason 确认（而非匹配错误文本），避免把 shader
            // 编译等普通渲染错误误判为设备丢失。PresenterRuntime 据此自愈。
            self.device_lost = true;
            let reason = self
                .state
                .as_ref()
                .and_then(|state| {
                    unsafe { state.device.GetDeviceRemovedReason() }
                        .err()
                        .map(|e| e.code().0)
                })
                .unwrap_or(0);
            self.stats.device_lost_events += 1;
            self.stats.last_device_lost_reason = reason as u32;
            if let Err(error) = &result {
                eprintln!(
                    "[d3d11] device lost/reset: reason=0x{:08X} at: {error}",
                    reason,
                );
            } else {
                eprintln!(
                    "[d3d11] device lost/reset: reason=0x{:08X} (no frame to render)",
                    reason,
                );
            }
            trace("device lost/reset detected; requesting presenter recovery");
        } else if let Err(error) = &result {
            // 非设备丢失的渲染失败也要留痕（粉屏/黑屏排查入口）。
            eprintln!("[d3d11] render failed (no device lost): {error}");
        }
        result
    }

    fn device_lost(&self) -> bool {
        self.device_lost
    }

    fn runtime_stats(&self) -> RendererRuntimeStats {
        RendererRuntimeStats {
            surface_width: self.stats.surface_width,
            surface_height: self.stats.surface_height,
            rendered_frames: self.stats.rendered_frames,
            attached: self.stats.attached,
            prepared_overlay_frames: self.stats.prepared_overlay_frames,
            prepared_overlay_subtitle_planes: self.stats.prepared_overlay_subtitle_planes,
            danmaku_passes: self.stats.danmaku_passes,
            danmaku_draw_items: self.stats.danmaku_items,
            overlay_alpha_atlas_uploads: self.stats.overlay_alpha_atlas_uploads,
            overlay_alpha_atlas_reuses: self.stats.overlay_alpha_atlas_reuses,
            software_video_frames: self.stats.software_video_frames,
            hardware_video_frames: self.stats.hardware_video_frames,
            zero_copy_video_frames: self.stats.zero_copy_video_frames,
            direct_zero_copy_video_frames: self.stats.direct_zero_copy_video_frames,
            shared_handle_video_frames: self.stats.shared_handle_video_frames,
            cpu_video_frame_fallbacks: self.stats.cpu_video_frame_fallbacks,
            hdr_source_frames: self.stats.hdr_source_frames,
            hdr10_output_frames: self.stats.hdr10_output_frames,
            sdr_tonemap_frames: self.stats.sdr_tonemap_frames,
            hdr10_metadata_updates: self.stats.hdr10_metadata_updates,
            hdr10_metadata_failures: self.stats.hdr10_metadata_failures,
            hdr10_output_failures: self.stats.hdr10_output_failures,
            hdr10_output_active: self.stats.hdr10_output_active,
            upscaler_mode: self.upscaler_mode,
            upscaler_backend: if self.upscaler_mode.is_enabled() && self.state.is_none() {
                LumaUpscalerBackendStatus::Building
            } else {
                self.upscaler.status()
            },
            upscaler_fallbacks: self.upscaler.fallback_count(),
            upscaled_frames: self.upscaler.upscaled_frames(),
            last_upscaler_encode_duration: self.upscaler.last_encode_duration(),
            // UI 线程（宿主渲染回调）分段耗时，见 `D3d11RendererStats` 的字段文档。
            last_overlay_prepare_duration: self.stats.last_overlay_prepare_duration,
            last_danmaku_prepare_duration: self.stats.last_danmaku_prepare_duration,
            last_draw_video_duration: self.stats.last_draw_video_duration,
            last_draw_overlay_duration: self.stats.last_draw_overlay_duration,
            last_present_duration: self.stats.last_present_duration,
            present_submitted: self.stats.present_submitted,
            present_dropped: self.stats.present_dropped,
            present_calls: self.stats.present_calls,
            present_total: self.stats.present_total,
            present_max: self.stats.present_max,
            present_over_1ms: self.stats.present_over_1ms,
            present_over_8ms: self.stats.present_over_8ms,
            frame_stats_available: self.stats.frame_stats_available,
            display_refresh_hz_milli: self.stats.display_refresh_hz_milli,
            presents_per_refresh_milli: self.stats.presents_per_refresh_milli,
            shown_frames_per_refresh_milli: self.stats.shown_frames_per_refresh_milli,
            overlay_plane_uploads: self.stats.overlay_plane_uploads,
            overlay_plane_upload_bytes: self.stats.overlay_plane_upload_bytes,
            overlay_plane_upload_skips: self.stats.overlay_plane_upload_skips,
            last_wait_gpu_duration: self.stats.last_wait_gpu_duration,
            wait_gpu_calls: self.stats.wait_gpu_calls,
            slow_wait_gpu_count: self.stats.slow_wait_gpu_count,
            ..Default::default()
        }
    }

    fn output_status(&self) -> OutputRuntimeStatus {
        let hdr10_active = self.stats.hdr10_output_active;
        OutputRuntimeStatus {
            requested_mode: self.requested_output_mode,
            active_encoding: if hdr10_active {
                ActiveOutputEncoding::Hdr10Pq
            } else {
                ActiveOutputEncoding::SdrSrgb
            },
            surface_format: if hdr10_active {
                OutputSurfaceFormat::TenBitUnorm
            } else {
                OutputSurfaceFormat::EightBitUnorm
            },
            native_data_space: -1,
            requested_headroom: self.requested_output_mode.headroom(),
            active_headroom: if hdr10_active { 10_000.0 / 203.0 } else { 1.0 },
            active_headroom_known: self.stats.attached,
            extended_linear_active: false,
            fallback_reason: OutputFallbackReason::None,
            fallback_count: self.stats.hdr10_output_failures,
            data_space_failures: self.stats.hdr10_output_failures,
            headroom_updates: self.stats.hdr10_metadata_updates,
            extended_linear_frames: 0,
        }
    }

    fn set_luma_upscaler(&mut self, mode: LumaUpscalerMode) {
        self.upscaler_mode = mode;
        if let Some(state) = self.state.as_ref() {
            if let Err(error) = self.upscaler.set_mode(&state.device, mode) {
                report_upscaler_failure("mode_switch", mode, &error);
            }
        }
    }

    fn composition_swapchain_ptr(&self) -> Option<*mut std::ffi::c_void> {
        Some(Interface::as_raw(self.composition_swapchain()?))
    }

    fn composition_swapchain_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        let unknown: IUnknown = self.composition_swapchain()?.cast().ok()?;
        Some(unknown.into_raw())
    }

    fn windows_flutter_texture_iunknown(&self) -> Option<*mut std::ffi::c_void> {
        let surface = self.surface.as_ref()?;
        if !surface.flutter_texture {
            return None;
        }
        let unknown: IUnknown = surface.published_texture.as_ref()?.cast().ok()?;
        Some(unknown.into_raw())
    }
}

impl D3d11DeviceState {
    fn new(device: ID3D11Device, context: ID3D11DeviceContext) -> Result<Self> {
        let vertex_blob = compile_shader("vs_main", "vs_4_0")?;
        let pixel_blob = compile_shader("ps_main", "ps_4_0")?;
        let encode_pixel_blob = compile_shader("encode_ps_main", "ps_4_0")?;
        let overlay_vertex_blob =
            compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_vs_main", "vs_4_0")?;
        let overlay_pixel_blob =
            compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_ps_main", "ps_4_0")?;
        let vertex_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreateVertexShader(blob_bytes(&vertex_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreateVertexShader", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreateVertexShader returned null".to_string())
            })?
        };
        let pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&pixel_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreatePixelShader", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader returned null".to_string())
            })?
        };
        let encode_pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&encode_pixel_blob), None, Some(&mut shader))
                    .map_err(|error| d3d_error("ID3D11Device::CreatePixelShader(encode)", error))?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader(encode) returned null".to_string())
            })?
        };
        let overlay_vertex_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreateVertexShader(blob_bytes(&overlay_vertex_blob), None, Some(&mut shader))
                    .map_err(|error| {
                        d3d_error("ID3D11Device::CreateVertexShader(overlay)", error)
                    })?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer(
                    "d3d11: CreateVertexShader(overlay) returned null".to_string(),
                )
            })?
        };
        let overlay_pixel_shader = {
            let mut shader = None;
            unsafe {
                device
                    .CreatePixelShader(blob_bytes(&overlay_pixel_blob), None, Some(&mut shader))
                    .map_err(|error| {
                        d3d_error("ID3D11Device::CreatePixelShader(overlay)", error)
                    })?;
            }
            shader.ok_or_else(|| {
                PlayerError::Renderer("d3d11: CreatePixelShader(overlay) returned null".to_string())
            })?
        };
        let input_layout = create_input_layout(&device, &vertex_blob)?;
        let vertex_buffer = create_vertex_buffer(&device)?;
        let constants = create_constants_buffer(&device)?;
        let sampler = create_sampler(&device)?;
        let overlay_constants = create_overlay_constants_buffer(&device)?;
        let overlay_sampler = create_sampler(&device)?;
        let overlay_blend = create_overlay_blend_state(&device)?;
        Ok(Self {
            device,
            context,
            vertex_shader,
            pixel_shader,
            encode_pixel_shader,
            input_layout,
            vertex_buffer,
            constants,
            sampler,
            overlay_vertex_shader,
            overlay_pixel_shader,
            overlay_constants,
            overlay_sampler,
            overlay_blend,
        })
    }

    /// Draw the imported video frame. `constants` is the per-frame uniform
    /// payload the caller already resolved (including any
    /// `gamut_lut_enabled` mask for a still-generating LUT); the stored
    /// `ImportedVideoFrame.constants` is left untouched so a later present
    /// can re-enable the LUT once it lands.
    fn draw_video(
        &self,
        video: &ImportedVideoFrame,
        constants: VideoUniforms,
        upscaled_luma: Option<&ID3D11ShaderResourceView>,
        gamut_lut: Option<&ID3D11ShaderResourceView>,
        render_target: &ID3D11RenderTargetView,
        target: D3d11DrawRect,
    ) -> Result<()> {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: target.x,
            TopLeftY: target.y,
            Width: target.width.max(1.0),
            Height: target.height.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let vertices = video_vertices(D3d11TexRect::FULL);
        let mut common = constants;
        if upscaled_luma.is_some() {
            common = common.packed_d2s_luma_input();
        }
        let constants = D3d11VideoConstants {
            common,
            texture_scales: [
                if upscaled_luma.is_some() {
                    1.0
                } else {
                    video.tex_rect.width
                },
                if upscaled_luma.is_some() {
                    1.0
                } else {
                    video.tex_rect.height
                },
                video.tex_rect.width,
                video.tex_rect.height,
            ],
        };
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &constants as *const _ as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.pixel_shader, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            let srvs = [
                Some(upscaled_luma.cloned().unwrap_or_else(|| video.luma.clone())),
                Some(video.chroma.clone()),
                gamut_lut.cloned(),
            ];
            self.context.PSSetShaderResources(0, Some(&srvs));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context.Draw(6, 0);
            self.context
                .PSSetShaderResources(0, Some(&[None, None, None]));
        }
        Ok(())
    }

    fn draw_overlays(
        &self,
        draws: &[D3d11OverlayDraw],
        render_target: &ID3D11RenderTargetView,
        target: D3d11DrawRect,
    ) -> Result<()> {
        if draws.is_empty() {
            return Ok(());
        }
        let viewport = D3D11_VIEWPORT {
            TopLeftX: target.x,
            TopLeftY: target.y,
            Width: target.width.max(1.0),
            Height: target.height.max(1.0),
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let blend_factor = [0.0f32; 4];
        let vertices = video_vertices(D3d11TexRect::FULL);
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.overlay_vertex_shader, None);
            self.context.PSSetShader(&self.overlay_pixel_shader, None);
            self.context
                .VSSetConstantBuffers(0, Some(&[Some(self.overlay_constants.clone())]));
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.overlay_constants.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.overlay_sampler.clone())]));
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context
                .OMSetBlendState(&self.overlay_blend, Some(&blend_factor), u32::MAX);
            for draw in draws {
                self.context.UpdateSubresource(
                    &self.overlay_constants,
                    0,
                    None,
                    &draw.constants as *const _ as *const c_void,
                    0,
                    0,
                );
                self.context
                    .PSSetShaderResources(0, Some(&[Some(draw.texture.view.clone())]));
                self.context.Draw(6, 0);
            }
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetBlendState(None, None, u32::MAX);
        }
        Ok(())
    }

    fn draw_encode(
        &self,
        scene: &ID3D11ShaderResourceView,
        render_target: &ID3D11RenderTargetView,
        width: u32,
        height: u32,
        mut constants: VideoUniforms,
    ) -> Result<()> {
        constants.scene_linear = 0;
        let constants = D3d11VideoConstants {
            common: constants,
            texture_scales: [1.0; 4],
        };
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width.max(1) as f32,
            Height: height.max(1) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let stride = mem::size_of::<VideoVertex>() as u32;
        let offset = 0u32;
        let vertices = video_vertices(D3d11TexRect::FULL);
        unsafe {
            self.context.UpdateSubresource(
                &self.vertex_buffer,
                0,
                None,
                vertices.as_ptr() as *const c_void,
                0,
                0,
            );
            self.context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &constants as *const _ as *const c_void,
                0,
                0,
            );
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(&self.input_layout);
            self.context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.encode_pixel_shader, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            // Unbind the scene texture as an RTV before exposing it as an SRV.
            self.context
                .OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            self.context.OMSetBlendState(None, None, u32::MAX);
            self.context
                .PSSetShaderResources(0, Some(&[Some(scene.clone())]));
            self.context.Draw(6, 0);
            // The same texture becomes an RTV again on the next frame.
            self.context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum D3d11VideoTextureFormat {
    Nv12,
    P010,
}

impl D3d11VideoTextureFormat {
    fn from_dxgi(format: DXGI_FORMAT) -> Option<Self> {
        if format == DXGI_FORMAT_NV12 {
            Some(Self::Nv12)
        } else if format == DXGI_FORMAT_P010 {
            Some(Self::P010)
        } else {
            None
        }
    }

    fn luma_srv(self) -> DXGI_FORMAT {
        match self {
            Self::Nv12 => DXGI_FORMAT_R8_UNORM,
            Self::P010 => DXGI_FORMAT_R16_UNORM,
        }
    }

    fn chroma_srv(self) -> DXGI_FORMAT {
        match self {
            Self::Nv12 => DXGI_FORMAT_R8G8_UNORM,
            Self::P010 => DXGI_FORMAT_R16G16_UNORM,
        }
    }
}

fn aspect_fit_rect(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> D3d11DrawRect {
    let rect =
        PresentationLayout::aspect_fit(source_width, source_height, target_width, target_height)
            .presentation_rect();
    D3d11DrawRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn d3d_pq_code_for_lut(nits: f32) -> f32 {
    let m1 = 0.1593017578125_f32;
    let m2 = 78.84375_f32;
    let c1 = 0.8359375_f32;
    let c2 = 18.8515625_f32;
    let c3 = 18.6875_f32;
    let p = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p).max(0.000_001)).powf(m2)
}

/// Quantize a luminance (nits) to its PQ code for the LUT cache key, matching
/// the wgpu/Metal quantizers. 16-bit PQ resolution keeps the sub-1-nit target
/// blacks of SDR targets distinct.
fn d3d_quantize_luma_pq(nits: f32) -> u32 {
    (d3d_pq_code_for_lut(nits) * 65535.0) as u32
}

fn d3d_code_to_primaries(code: u32) -> ColorPrimaries {
    match code {
        1 => ColorPrimaries::Bt2020,
        2 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Bt709,
    }
}

fn create_default_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    trace("create_default_device: D3D11CreateDevice");
    let feature_levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut device = None;
    let mut context = None;
    let mut selected = D3D_FEATURE_LEVEL(0);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected),
            Some(&mut context),
        )
        .map_err(|error| d3d_error("D3D11CreateDevice", error))?;
    }
    let device =
        device.ok_or_else(|| PlayerError::Renderer("d3d11: device was null".to_string()))?;
    let context =
        context.ok_or_else(|| PlayerError::Renderer("d3d11: context was null".to_string()))?;
    trace("create_default_device: done");
    Ok((device, context))
}

/// `ISwapChainPanelNative::SetSwapChain` 的最小 COM 绑定。
/// crates.io 的 windows 0.62.2 未导出该接口（GUID
/// f92f19d2-3ade-45a6-a20c-f6f1ea90554b），手写 vtbl 调用保持零依赖改动。
#[repr(C)]
struct ISwapChainPanelNativeVtbl {
    base: ::windows::core::IUnknown_Vtbl,
    set_swap_chain: unsafe extern "system" fn(this: *mut c_void, swapchain: *const c_void) -> i32,
}

fn panel_set_swap_chain(panel: *mut c_void, swapchain: &IDXGISwapChain1) -> Result<()> {
    unsafe {
        let vtbl = &*((*(panel as *mut *mut c_void)) as *const ISwapChainPanelNativeVtbl);
        // 注意：COM 参数是接口指针本身（as_raw），不能传 &swapchain 的地址
        let hr = (vtbl.set_swap_chain)(panel, swapchain.as_raw() as *const c_void);
        let hresult = ::windows::core::HRESULT(hr);
        if hresult.is_err() {
            return Err(crate::core::PlayerError::Renderer(format!(
                "ISwapChainPanelNative::SetSwapChain failed with {:?}",
                hresult
            )));
        }
    }
    Ok(())
}

/// `ISwapChainPanelNative::SetSwapChain(null)`：把 swapchain 从 panel 上摘除。
///
/// 释放旧 swapchain 前**必须先摘除**。panel 持有对 swapchain 的引用，直接 drop
/// Rust 侧的 swapchain 只是引用计数 -1，panel 那份引用仍在；而 render 循环的
/// present 走 `DXGI_PRESENT_DO_NOT_WAIT`（异步提交、最多 1 帧在飞），resize/换源
/// 重建 swapchain 时若那 1 帧仍被合成器消费旧 backbuffer，drop 旧 swapchain 即
/// use-after-free → `ucrtbase.dll` 0xc0000005（拖拽窗口/点播放闪退根因）。
/// SetSwapChain(null) 让 panel 先断开对旧 swapchain 的引用、合成器不再消费，
/// 之后再 drop 才安全。
fn panel_clear_swap_chain(panel: *mut c_void) -> Result<()> {
    unsafe {
        let vtbl = &*((*(panel as *mut *mut c_void)) as *const ISwapChainPanelNativeVtbl);
        let hr = (vtbl.set_swap_chain)(panel, std::ptr::null());
        let hresult = ::windows::core::HRESULT(hr);
        if hresult.is_err() {
            return Err(crate::core::PlayerError::Renderer(format!(
                "ISwapChainPanelNative::SetSwapChain(null) failed with {:?}",
                hresult
            )));
        }
    }
    Ok(())
}

fn create_swapchain(
    device: &ID3D11Device,
    hwnd: HWND,
    panel: Option<*mut c_void>,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    composition: bool,
    premultiplied_alpha: bool,
) -> Result<IDXGISwapChain1> {
    trace("create_swapchain: cast IDXGIDevice");
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| d3d_error("ID3D11Device::cast<IDXGIDevice>", error))?;
    trace("create_swapchain: get adapter");
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|error| d3d_error("IDXGIDevice::GetAdapter", error))?;
    trace("create_swapchain: get factory");
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
        .map_err(|error| d3d_error("IDXGIAdapter::GetParent<IDXGIFactory2>", error))?;

    let mut desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width.max(1),
        Height: height.max(1),
        Format: format,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: if premultiplied_alpha {
            DXGI_ALPHA_MODE_PREMULTIPLIED
        } else {
            DXGI_ALPHA_MODE_IGNORE
        },
        Flags: 0,
    };
    // WinUI 3 SwapChainPanel 必须是 composition swap chain，且要求
    // FLIP_SEQUENTIAL + PREMULTIPLIED alpha；DirectComposition（透明视频）走
    // composition 分支，alpha 模式由 premultiplied_alpha 决定。
    if panel.is_some() {
        desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        desc.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
    }
    let swapchain = if composition || panel.is_some() {
        trace("create_swapchain: CreateSwapChainForComposition");
        unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }
            .map_err(|error| d3d_error("IDXGIFactory2::CreateSwapChainForComposition", error))?
    } else {
        trace("create_swapchain: CreateSwapChainForHwnd");
        unsafe { factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }
            .map_err(|error| d3d_error("IDXGIFactory2::CreateSwapChainForHwnd", error))?
    };
    if let Some(panel) = panel {
        trace("create_swapchain: ISwapChainPanelNative::SetSwapChain");
        panel_set_swap_chain(panel, &swapchain)?;
    }
    trace("create_swapchain: done");
    Ok(swapchain)
}

fn configure_swapchain_color_space(
    swapchain: &IDXGISwapChain1,
    output_mode: D3d11OutputMode,
) -> Result<()> {
    let swapchain3: IDXGISwapChain3 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain3>", error))?;
    let color_space = output_mode.color_space();
    if matches!(output_mode, D3d11OutputMode::Hdr10) {
        let support = unsafe { swapchain3.CheckColorSpaceSupport(color_space) }
            .map_err(|error| d3d_error("IDXGISwapChain3::CheckColorSpaceSupport(HDR10)", error))?;
        if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 == 0 {
            return Err(PlayerError::Renderer(
                "d3d11: HDR10 swapchain color space is not presentable".to_string(),
            ));
        }
    }
    unsafe { swapchain3.SetColorSpace1(color_space) }
        .map_err(|error| d3d_error("IDXGISwapChain3::SetColorSpace1", error))
}

fn set_hdr10_metadata(
    swapchain: &IDXGISwapChain1,
    metadata: DXGI_HDR_METADATA_HDR10,
) -> Result<()> {
    let swapchain4: IDXGISwapChain4 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain4>", error))?;
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&metadata as *const DXGI_HDR_METADATA_HDR10).cast::<u8>(),
            mem::size_of::<DXGI_HDR_METADATA_HDR10>(),
        )
    };
    unsafe { swapchain4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes)) }
        .map_err(|error| d3d_error("IDXGISwapChain4::SetHDRMetaData(HDR10)", error))
}

fn clear_hdr_metadata(swapchain: &IDXGISwapChain1) -> Result<()> {
    let swapchain4: IDXGISwapChain4 = swapchain
        .cast()
        .map_err(|error| d3d_error("IDXGISwapChain1::cast<IDXGISwapChain4>", error))?;
    unsafe { swapchain4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_NONE, None) }
        .map_err(|error| d3d_error("IDXGISwapChain4::SetHDRMetaData(None)", error))
}

fn release_backbuffer_views(context: &ID3D11DeviceContext, surface: &mut AttachedSurface) {
    // ResizeBuffers / creating a new chain requires every RTV and SRV of the
    // current back buffer to be gone, including those bound on the immediate
    // context.
    unsafe {
        context.OMSetRenderTargets(None, None);
        context.PSSetShaderResources(0, Some(&[None, None]));
    }
    surface.render_target = None;
    surface.output_texture = None;
    surface.published_texture = None;
    surface.linear_render_target = None;
    surface.linear_shader_resource = None;
}

/// 创建栅栏用的 `D3D11_QUERY_EVENT` 对象。
///
/// **只在 [`D3d11Renderer::wait_for_gpu`] 的复用槽为空时调用**：稳态播放中应当
/// 只在设备重建后命中一次（计数见 `D3d11RendererStats::gpu_fence_query_creates`）。
/// 历史实现是每个上屏视频帧都调用它一次，见 `wait_for_gpu` 的「query 复用」一节。
fn create_d3d11_event_query(device: &ID3D11Device) -> Result<ID3D11Query> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    unsafe {
        device
            .CreateQuery(&desc, Some(&mut query))
            .map_err(|error| d3d_error("ID3D11Device::CreateQuery(video retirement)", error))?;
    }
    query.ok_or_else(|| PlayerError::Renderer("d3d11: video retirement query was null".to_string()))
}

/// 非阻塞探测：query 是否已完成（`D3D11_ASYNC_GETDATA_DONOTFLUSH`）。
///
/// 热路径**不再使用**它：`wait_for_gpu` 已改为阻塞式 `GetData`（见那里的文档，
/// 轮询 sleep 撞系统时钟粒度是历史卡顿根因）。保留它是因为它是本文件唯一一处
/// 演示 `D3D11_ASYNC_GETDATA_DONOTFLUSH` 语义的实现，后续做「栅栏是否为空」
/// 这类零等待探测时可直接复用，不必重新推导标志位含义。
#[allow(dead_code)]
fn d3d11_event_query_complete(context: &ID3D11DeviceContext, query: &ID3D11Query) -> Result<bool> {
    let mut complete = BOOL::default();
    unsafe {
        context
            .GetData(
                query,
                Some((&mut complete as *mut BOOL).cast::<c_void>()),
                mem::size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
            .map_err(|error| d3d_error("ID3D11DeviceContext::GetData(video retirement)", error))?;
    }
    Ok(complete.as_bool())
}

/// 栅栏等待超过此值时计入 [`D3d11RendererStats::slow_wait_gpu_count`]。
///
/// 取值 2ms 的依据：阻塞式 `GetData` 在稳态下应当 **<1ms** —— GPU 队列此时通常
/// 已经空了（上一拍的命令早执行完），驱动几乎立刻返回。一旦超过 2ms，就说明
/// 要么 GPU 真的排了很深的队，要么实现退化回了轮询等待
/// （历史缺陷下是 **15.5ms**，撞 Windows 默认 15.625ms 时钟粒度）。
///
/// **这是回归告警线，不是性能目标**：它存在的唯一目的是让"有人把
/// `wait_for_gpu` 改回 sleep 轮询"这件事**在诊断面板上立刻可见**
/// （`慢栅栏` 计数开始增长），而不是只能靠用户体感重新发现一次卡顿。
const GPU_WAIT_SLOW_THRESHOLD: Duration = Duration::from_millis(2);

/// 栅栏等待是否算"慢"（回归信号）。见 [`GPU_WAIT_SLOW_THRESHOLD`]。
fn is_slow_gpu_wait(waited: Duration) -> bool {
    waited > GPU_WAIT_SLOW_THRESHOLD
}

fn create_flutter_texture_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: SDR_SWAPCHAIN_FORMAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(Flutter)", error))?;
    }
    let texture = texture.ok_or_else(|| {
        PlayerError::Renderer("d3d11: Flutter output texture was null".to_string())
    })?;
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>(Flutter)", error))?;
    let mut render_target = None;
    unsafe {
        device
            .CreateRenderTargetView(&resource, None, Some(&mut render_target))
            .map_err(|error| d3d_error("ID3D11Device::CreateRenderTargetView(Flutter)", error))?;
    }
    Ok((
        texture,
        render_target.ok_or_else(|| {
            PlayerError::Renderer("d3d11: Flutter render target view was null".to_string())
        })?,
    ))
}

fn create_render_target(
    device: &ID3D11Device,
    swapchain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView> {
    trace("create_render_target: GetBuffer");
    let back_buffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }
        .map_err(|error| d3d_error("IDXGISwapChain1::GetBuffer", error))?;
    trace("create_render_target: cast resource");
    let resource: ID3D11Resource = back_buffer
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let mut view = None;
    unsafe {
        trace("create_render_target: CreateRenderTargetView");
        device
            .CreateRenderTargetView(&resource, None, Some(&mut view))
            .map_err(|error| d3d_error("ID3D11Device::CreateRenderTargetView", error))?;
    }
    let view =
        view.ok_or_else(|| PlayerError::Renderer("d3d11: render target was null".to_string()))?;
    trace("create_render_target: done");
    Ok(view)
}

fn create_linear_render_target(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(linear target)", error))?;
    }
    let texture = texture.ok_or_else(|| {
        PlayerError::Renderer("d3d11: linear render target texture was null".to_string())
    })?;
    let resource: ID3D11Resource = texture.cast().map_err(|error| {
        d3d_error(
            "ID3D11Texture2D::cast<ID3D11Resource>(linear target)",
            error,
        )
    })?;
    let mut render_target = None;
    let mut shader_resource = None;
    unsafe {
        device
            .CreateRenderTargetView(&resource, None, Some(&mut render_target))
            .map_err(|error| {
                d3d_error("ID3D11Device::CreateRenderTargetView(linear target)", error)
            })?;
        device
            .CreateShaderResourceView(&resource, None, Some(&mut shader_resource))
            .map_err(|error| {
                d3d_error(
                    "ID3D11Device::CreateShaderResourceView(linear target)",
                    error,
                )
            })?;
    }
    Ok((
        render_target.ok_or_else(|| {
            PlayerError::Renderer("d3d11: linear render target view was null".to_string())
        })?,
        shader_resource.ok_or_else(|| {
            PlayerError::Renderer("d3d11: linear shader resource view was null".to_string())
        })?,
    ))
}

/// `Present1` 的结果：真正提交了，还是因队列满被丢弃。
///
/// 分成两种而不是 `Result<()>`，是为了让调用方能把「本拍没上屏」计入诊断
/// 计数器 —— 见 [`D3d11RendererStats::present_dropped`]。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentOutcome {
    /// 命令已进 DXGI 队列，本拍内容会（在某个 vblank）上屏。
    Submitted,
    /// `DXGI_ERROR_WAS_STILL_DRAWING`：队列仍被上一拍占用，本拍丢弃。
    ///
    /// 丢弃是**故意的**：UI 线程同时服务 `WM_MOVING`/`WM_SIZING`，在这里等
    /// 一个 vblank 会直接把窗口拖拽变成「延迟 16ms 不跟手」。丢帧不会丢内容
    /// —— 交换链保持上一拍，下一拍只要内容还是新的就会再画一次。
    Dropped,
}

fn present_swapchain(
    swapchain: &IDXGISwapChain1,
    operation: &'static str,
) -> Result<PresentOutcome> {
    trace("present_swapchain: enter");
    let params = DXGI_PRESENT_PARAMETERS {
        DirtyRectsCount: 0,
        pDirtyRects: ptr::null_mut(),
        pScrollRect: ptr::null_mut(),
        pScrollOffset: ptr::null_mut(),
    };
    let status = unsafe { swapchain.Present1(0, DXGI_PRESENT_DO_NOT_WAIT, &params) };
    // Rendering shares the Windows platform thread with WM_MOVING. Never wait
    // for a full DXGI queue here: dropping one presentation keeps window
    // interaction responsive while decode/audio continue normally.
    if status == DXGI_ERROR_WAS_STILL_DRAWING {
        return Ok(PresentOutcome::Dropped);
    }
    status.ok().map_err(|error| d3d_error(operation, error))?;
    Ok(PresentOutcome::Submitted)
}

fn create_plane_srv(
    state: &D3d11DeviceState,
    texture: &ID3D11Texture2D,
    array_index: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: array_index,
                ArraySize: 1,
            },
        },
    };
    let mut view = None;
    unsafe {
        state
            .device
            .CreateShaderResourceView(&resource, Some(&desc), Some(&mut view))
            .map_err(|error| {
                d3d_error(
                    "ID3D11Device::CreateShaderResourceView(D3D11VA plane)",
                    error,
                )
            })?;
    }
    view.ok_or_else(|| PlayerError::Renderer("d3d11: shader resource view was null".to_string()))
}

/// 跨设备共享纹理导入（解码器设备 → 渲染器设备）。当前渲染器与解码器
/// 已统一为同一设备（DirectDecoderDevice），此路径保留备用（测试/回退）。
/// SHARED 纹理跨设备访问无自动 GPU 同步，并发读写会触发 TDR/花屏，勿复用。
#[allow(dead_code)]
fn open_shared_texture_on_device(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11Texture2D> {
    let shared_handle = d3d11_shared_handle(texture)?;
    let mut imported = None;
    unsafe {
        device
            .OpenSharedResource::<ID3D11Texture2D>(shared_handle, &mut imported)
            .map_err(|error| d3d_error("ID3D11Device::OpenSharedResource(D3D11VA)", error))?;
    }
    imported
        .ok_or_else(|| PlayerError::Renderer("d3d11: shared texture import was null".to_string()))
}

#[allow(dead_code)]
fn d3d11_shared_handle(texture: &ID3D11Texture2D) -> Result<HANDLE> {
    let resource: IDXGIResource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<IDXGIResource>", error))?;
    let handle = unsafe { resource.GetSharedHandle() }
        .map_err(|error| d3d_error("IDXGIResource::GetSharedHandle(D3D11VA)", error))?;
    if handle.is_invalid() {
        return Err(PlayerError::Renderer(
            "d3d11: D3D11VA texture did not expose a valid shared handle".to_string(),
        ));
    }
    Ok(handle)
}

fn create_cpu_texture(
    state: &D3d11DeviceState,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    data: &[u8],
    bytes_per_row: u32,
) -> Result<D3d11OverlayTexture> {
    if width == 0 || height == 0 || bytes_per_row == 0 {
        return Err(PlayerError::Renderer(
            "d3d11: CPU texture dimensions must be non-zero".to_string(),
        ));
    }
    let required_len = (bytes_per_row as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PlayerError::Renderer("d3d11: CPU texture size overflow".into()))?;
    if data.len() < required_len {
        return Err(PlayerError::Renderer(format!(
            "d3d11: CPU texture has {} bytes, expected at least {required_len}",
            data.len()
        )));
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let subresource = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const c_void,
        SysMemPitch: bytes_per_row,
        SysMemSlicePitch: bytes_per_row.saturating_mul(height),
    };
    let mut texture = None;
    unsafe {
        state
            .device
            .CreateTexture2D(&desc, Some(&subresource), Some(&mut texture))
            .map_err(|error| d3d_error("ID3D11Device::CreateTexture2D(cpu)", error))?;
    }
    let texture =
        texture.ok_or_else(|| PlayerError::Renderer("d3d11: CPU texture was null".to_string()))?;
    let view = create_texture2d_srv(state, &texture, format)?;
    Ok(D3d11OverlayTexture {
        _texture: texture,
        view,
    })
}

fn create_texture2d_srv(
    state: &D3d11DeviceState,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView> {
    let resource: ID3D11Resource = texture
        .cast()
        .map_err(|error| d3d_error("ID3D11Texture2D::cast<ID3D11Resource>", error))?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: ::windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut view = None;
    unsafe {
        state
            .device
            .CreateShaderResourceView(&resource, Some(&desc), Some(&mut view))
            .map_err(|error| d3d_error("ID3D11Device::CreateShaderResourceView(overlay)", error))?;
    }
    view.ok_or_else(|| PlayerError::Renderer("d3d11: overlay SRV was null".to_string()))
}

fn create_input_layout(device: &ID3D11Device, vertex_blob: &ID3DBlob) -> Result<ID3D11InputLayout> {
    const POSITION: &[u8] = b"POSITION\0";
    const TEXCOORD: &[u8] = b"TEXCOORD\0";
    let elements = [
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(POSITION.as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D11_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(TEXCOORD.as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 8,
            InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
    let mut layout = None;
    unsafe {
        device
            .CreateInputLayout(&elements, blob_bytes(vertex_blob), Some(&mut layout))
            .map_err(|error| d3d_error("ID3D11Device::CreateInputLayout", error))?;
    }
    layout.ok_or_else(|| PlayerError::Renderer("d3d11: input layout was null".to_string()))
}

fn create_vertex_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let vertices = video_vertices(D3d11TexRect::FULL);
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of_val(&vertices) as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: ::windows::Win32::Graphics::Direct3D11::D3D11_BIND_VERTEX_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: vertices.as_ptr() as *const c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    create_buffer(
        device,
        &desc,
        Some(&data),
        "ID3D11Device::CreateBuffer(vertex)",
    )
}

fn video_vertices(tex_rect: D3d11TexRect) -> [VideoVertex; 6] {
    let left = tex_rect.x;
    let right = tex_rect.right();
    let top = tex_rect.y;
    let bottom = tex_rect.bottom();
    [
        VideoVertex {
            position: [-1.0, -1.0],
            texcoord: [left, bottom],
        },
        VideoVertex {
            position: [-1.0, 1.0],
            texcoord: [left, top],
        },
        VideoVertex {
            position: [1.0, -1.0],
            texcoord: [right, bottom],
        },
        VideoVertex {
            position: [1.0, -1.0],
            texcoord: [right, bottom],
        },
        VideoVertex {
            position: [-1.0, 1.0],
            texcoord: [left, top],
        },
        VideoVertex {
            position: [1.0, 1.0],
            texcoord: [right, top],
        },
    ]
}

fn create_constants_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of::<D3d11VideoConstants>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    create_buffer(device, &desc, None, "ID3D11Device::CreateBuffer(constants)")
}

fn create_overlay_constants_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: mem::size_of::<OverlayUniforms>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    create_buffer(
        device,
        &desc,
        None,
        "ID3D11Device::CreateBuffer(overlay constants)",
    )
}

fn create_overlay_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let target = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D11_BLEND_SRC_ALPHA,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut render_targets = [D3D11_RENDER_TARGET_BLEND_DESC::default(); 8];
    render_targets[0] = target;
    let desc = D3D11_BLEND_DESC {
        AlphaToCoverageEnable: false.into(),
        IndependentBlendEnable: false.into(),
        RenderTarget: render_targets,
    };
    let mut state = None;
    unsafe {
        device
            .CreateBlendState(&desc, Some(&mut state))
            .map_err(|error| d3d_error("ID3D11Device::CreateBlendState(overlay)", error))?;
    }
    state.ok_or_else(|| PlayerError::Renderer("d3d11: overlay blend state was null".to_string()))
}

fn create_buffer(
    device: &ID3D11Device,
    desc: &D3D11_BUFFER_DESC,
    data: Option<&D3D11_SUBRESOURCE_DATA>,
    operation: &'static str,
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device
            .CreateBuffer(desc, data.map(|data| data as *const _), Some(&mut buffer))
            .map_err(|error| d3d_error(operation, error))?;
    }
    buffer.ok_or_else(|| PlayerError::Renderer(format!("d3d11: {operation} returned null")))
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: ::windows::Win32::Graphics::Direct3D11::D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: ::windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: ::windows::Win32::Graphics::Direct3D11::D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut sampler = None;
    unsafe {
        device
            .CreateSamplerState(&desc, Some(&mut sampler))
            .map_err(|error| d3d_error("ID3D11Device::CreateSamplerState", error))?;
    }
    sampler.ok_or_else(|| PlayerError::Renderer("d3d11: sampler was null".to_string()))
}

fn compile_shader(entry: &'static str, target: &'static str) -> Result<ID3DBlob> {
    compile_shader_source(SHADER_SOURCE, entry, target)
}

fn compile_shader_source(
    source: &'static [u8],
    entry: &'static str,
    target: &'static str,
) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let entry = nul(entry);
    let target = nul(target);
    unsafe {
        let result = D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = result {
            let message = errors
                .as_ref()
                .map(blob_to_string)
                .unwrap_or_else(|| error.message());
            return Err(PlayerError::Renderer(format!(
                "d3d11: D3DCompile({}/{}) failed: {message}",
                String::from_utf8_lossy(&entry[..entry.len() - 1]),
                String::from_utf8_lossy(&target[..target.len() - 1])
            )));
        }
    }
    code.ok_or_else(|| PlayerError::Renderer("d3d11: D3DCompile returned null".to_string()))
}

fn clone_d3d11_texture(raw: *mut c_void) -> Result<ID3D11Texture2D> {
    if raw.is_null() {
        return Err(PlayerError::Renderer(
            "d3d11: D3D11VA texture pointer is null".to_string(),
        ));
    }
    let borrowed = unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }.ok_or_else(|| {
        PlayerError::Renderer("d3d11: failed to borrow D3D11VA texture".to_string())
    })?;
    Ok(borrowed.clone())
}

fn source_color_for_frame(frame: &PlayerVideoFrame) -> SourceColorState {
    SourceColorState::new(
        frame.frame.color_primaries(),
        frame.frame.transfer_function(),
    )
    .range(frame.frame.color_range())
    .matrix(frame.frame.matrix_coefficients())
    .hdr_metadata(frame.frame.hdr_metadata())
    .dovi(frame.frame.dovi_metadata())
    .measured_scene_avg_nits(frame.scene_avg_nits)
}

fn constants_for_frame(
    source: SourceColorState,
    texture_format: D3d11VideoTextureFormat,
    target: TargetColorState,
) -> VideoUniforms {
    let pipeline = VideoRenderPipeline::new(source, target);
    let uniforms = VideoUniforms::from_pipeline(
        &pipeline,
        matches!(texture_format, D3d11VideoTextureFormat::P010),
        false,
    );
    if matches!(target.transfer, TransferFunction::Pq) {
        uniforms.scene_linear_output()
    } else {
        uniforms
    }
}

fn dxgi_hdr10_metadata(source: SourceColorState) -> DXGI_HDR_METADATA_HDR10 {
    let mastering = source
        .hdr_metadata
        .and_then(|metadata| metadata.mastering_display);
    let content_light = source
        .hdr_metadata
        .and_then(|metadata| metadata.content_light);
    let default_primaries = primaries_coordinates(ColorPrimaries::Bt2020);
    let primaries = mastering
        .and_then(|metadata| metadata.display_primaries)
        .map(|primaries| PrimariesCoordinates {
            red: primaries[0],
            green: primaries[1],
            blue: primaries[2],
            white: mastering
                .and_then(|metadata| metadata.white_point)
                .unwrap_or(default_primaries.white),
        })
        .unwrap_or(default_primaries);
    let max_mastering = mastering
        .and_then(|metadata| metadata.max_luminance_nits)
        .unwrap_or(source.nominal_peak_nits.max(1000.0));
    let min_mastering = mastering
        .and_then(|metadata| metadata.min_luminance_nits)
        .unwrap_or(0.005);
    let max_cll = content_light
        .map(|metadata| metadata.max_content_light_level_nits)
        .unwrap_or_else(|| max_mastering.round().max(1.0) as u32);
    let max_fall = content_light
        .map(|metadata| metadata.max_frame_average_light_level_nits)
        .unwrap_or_else(|| (max_mastering * 0.4).round().max(1.0) as u32);

    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: dxgi_chromaticity(primaries.red),
        GreenPrimary: dxgi_chromaticity(primaries.green),
        BluePrimary: dxgi_chromaticity(primaries.blue),
        WhitePoint: dxgi_chromaticity(primaries.white),
        MaxMasteringLuminance: nits_u32(max_mastering),
        MinMasteringLuminance: min_mastering_luminance(min_mastering),
        MaxContentLightLevel: clamp_u16(max_cll),
        MaxFrameAverageLightLevel: clamp_u16(max_fall),
    }
}

fn dxgi_chromaticity(value: Chromaticity) -> [u16; 2] {
    [chromaticity_coord(value.x), chromaticity_coord(value.y)]
}

fn chromaticity_coord(value: f32) -> u16 {
    if value.is_finite() {
        (value.clamp(0.0, 1.0) * 50_000.0)
            .round()
            .clamp(0.0, u16::MAX as f32) as u16
    } else {
        0
    }
}

fn nits_u32(value: f32) -> u32 {
    if value.is_finite() {
        value.round().clamp(1.0, u32::MAX as f32) as u32
    } else {
        1000
    }
}

fn min_mastering_luminance(value: f32) -> u32 {
    if value.is_finite() {
        (value.max(0.0) * 10_000.0)
            .round()
            .clamp(0.0, u32::MAX as f32) as u32
    } else {
        50
    }
}

fn clamp_u16(value: u32) -> u16 {
    value.min(u16::MAX as u32) as u16
}

fn nul(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn blob_to_string(blob: &ID3DBlob) -> String {
    String::from_utf8_lossy(blob_bytes(blob)).into_owned()
}

/// ERIKA_D3D11_TRACE 门控缓存（进程内 env 不变）：trace() 在渲染热路径上
/// 每帧至少调用 1 次（render_video enter）+ 每次帧导入 1 次（D3D11VA 零拷
/// 贝）——此前每次调用都 env::var_os 查询（Windows 拿进程环境锁 + 线性查
/// 找），60fps 下 ~120 次/秒白拿锁。LazyLock 首次求值后零开销。
static D3D11_TRACE_ENABLED: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("ERIKA_D3D11_TRACE").is_some());

fn trace(message: &str) {
    if !*D3D11_TRACE_ENABLED {
        return;
    }
    eprintln!("erika d3d11: {message}");
    // 崩溃定位：同时写文件（GUI 宿主 stderr 不可见）
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("erika_d3d11_trace.log"))
    {
        let _ = writeln!(f, "{message}");
    }
}

fn report_upscaler_failure(stage: &str, mode: LumaUpscalerMode, error: &PlayerError) {
    eprintln!("Erika D3D11 ArtCNN disabled after {stage} failure: {error}");
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "luma_upscaler",
            "stage": stage,
            "renderer": "d3d11",
            "requestedMode": format!("{mode:?}"),
            "activeBackend": "inactive",
            "reason": error.to_string(),
            "fallback": "native_luma_sampling",
        })
        .to_string(),
    );
}

fn d3d11va_srv_error(
    error: PlayerError,
    desc: &D3D11_TEXTURE2D_DESC,
    array_index: u32,
) -> PlayerError {
    PlayerError::Renderer(format!(
        "{error}; texture desc format={:?} bind_flags=0x{:x} misc_flags=0x{:x} usage={:?} array_size={} mip_levels={} sample_count={} array_index={}",
        desc.Format,
        desc.BindFlags,
        desc.MiscFlags,
        desc.Usage,
        desc.ArraySize,
        desc.MipLevels,
        desc.SampleDesc.Count,
        array_index
    ))
}

fn d3d_error(operation: &'static str, error: ::windows::core::Error) -> PlayerError {
    PlayerError::Renderer(format!("d3d11: {operation} failed: {}", error.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归守卫：`slow_wait_gpu_count` 必须能把"轮询式退化"认出来。
    ///
    /// 这条测试锁的是**修复不被悄悄改回去**。历史上 `wait_for_gpu` 用
    /// `sleep(1ms)` 轮询，撞上 Windows 默认 15.625ms 的系统时钟粒度，于是每个
    /// 上屏视频帧都白等约 15.5ms —— 直接表现为 UI 线程（与 `WM_MOVING` 同一个
    /// 线程）每帧被占满，窗口拖拽/缩放发涩、120Hz 屏拉不满。
    ///
    /// 如果有人把实现改回轮询，等待时长会回到 15ms 量级；只要 `is_slow_gpu_wait`
    /// 仍把它判为"慢"，诊断面板上的 `慢栅栏` 计数就会立刻增长，使用者不必重新
    /// 体感一轮才发现回归。
    #[test]
    fn slow_gpu_wait_detects_poll_style_regression() {
        // 轮询撞 15.625ms 时钟粒度的典型实测值（真机日志 15.2~16.4ms）。
        assert!(
            is_slow_gpu_wait(Duration::from_micros(15_520)),
            "15.5ms 量级的栅栏等待必须被判为慢（这正是回归的指纹）"
        );
        assert!(
            is_slow_gpu_wait(Duration::from_micros(16_370)),
            "真机 max 观测值 16.37ms 必须被判为慢"
        );
        // 阻塞式 `GetData` 的稳态值：GPU 队列通常已空，几乎立即返回。
        assert!(
            !is_slow_gpu_wait(Duration::from_micros(300)),
            "亚毫秒的栅栏等待不得误报为慢"
        );
        assert!(!is_slow_gpu_wait(Duration::ZERO));
    }

    /// 阈值语义：恰好等于阈值不算慢（严格大于），保证"2ms 整"不会被误计。
    #[test]
    fn slow_gpu_wait_threshold_is_exclusive() {
        assert!(!is_slow_gpu_wait(GPU_WAIT_SLOW_THRESHOLD));
        assert!(is_slow_gpu_wait(
            GPU_WAIT_SLOW_THRESHOLD + Duration::from_micros(1)
        ));
    }

    fn software_test_renderer() -> D3d11Renderer {
        use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("create WARP device");
        }
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer
            .set_device(device.unwrap(), context.unwrap())
            .unwrap();
        renderer
    }

    #[test]
    fn software_p010_upload_preserves_odd_dimensions_and_sample_precision() {
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut renderer = software_test_renderer();
        let luma: Vec<u8> = [64u16, 128, 256, 512, 1000, 1023, 940, 128, 64]
            .into_iter()
            .flat_map(|value| (value << 6).to_le_bytes())
            .collect();
        let chroma: Vec<u8> = [64u16, 960, 128, 896, 512, 512, 960, 64]
            .into_iter()
            .flat_map(|value| (value << 6).to_le_bytes())
            .collect();
        renderer
            .upload_planar_frame(
                PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: 3,
                    height: 3,
                    luma: luma.clone(),
                    chroma: chroma.clone(),
                },
                SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq),
            )
            .unwrap();
        let video = renderer.current_video.as_ref().unwrap();
        assert_eq!(video.constants.is_p010, 1);
        assert_eq!(video.tex_rect, D3d11TexRect::FULL);
        let state = renderer.state.as_ref().unwrap();
        for (view, width, height, format, expected, pitch) in [
            (&video.luma, 3, 3, DXGI_FORMAT_R16_UNORM, luma, 6usize),
            (
                &video.chroma,
                2,
                2,
                DXGI_FORMAT_R16G16_UNORM,
                chroma,
                8usize,
            ),
        ] {
            unsafe {
                let texture: ID3D11Texture2D = view.GetResource().unwrap().cast().unwrap();
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                assert_eq!(
                    (desc.Width, desc.Height, desc.Format),
                    (width, height, format)
                );
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut staging = None;
                state
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut staging))
                    .unwrap();
                let staging = staging.unwrap();
                state.context.CopyResource(&staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                state
                    .context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .unwrap();
                let mut actual = Vec::new();
                for row in 0..height as usize {
                    actual.extend_from_slice(std::slice::from_raw_parts(
                        mapped
                            .pData
                            .cast::<u8>()
                            .add(row * mapped.RowPitch as usize),
                        pitch,
                    ));
                }
                state.context.Unmap(&staging, 0);
                assert_eq!(actual, expected);
            }
        }
        assert_eq!(renderer.runtime_stats().software_video_frames, 1);
        assert_eq!(renderer.runtime_stats().hdr_source_frames, 1);
    }

    #[test]
    fn software_wmv_uploads_to_native_d3d11() {
        use crate::ffmpeg::{DecoderBackend, DecoderOutputFrame, Demuxer, StreamSelection};
        use crate::renderer::VideoFramePayload;
        let mut renderer = software_test_renderer();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/software/wmv2-wmav2.asf");
        let mut demuxer = Demuxer::open_path(path).unwrap();
        demuxer
            .set_stream_selection(StreamSelection::only([0]))
            .unwrap();
        let mut decoder = demuxer.open_decoder(0).unwrap();
        let mut uploads = 0;
        while let Some(packet) = demuxer.read_packet().unwrap() {
            decoder.send_packet(&packet).unwrap();
            while let DecoderOutputFrame::Frame(frame) = decoder.receive_frame().unwrap() {
                let player_frame = PlayerVideoFrame {
                    frame: VideoFramePayload::from_decoded(frame).unwrap(),
                    decode_backend: DecoderBackend::Software,
                    pts: None,
                    media_time: Duration::ZERO,
                    late_by: None,
                    generation: 0,
                    scene_avg_nits: None,
                };
                renderer.upload_player_frame(&player_frame).unwrap();
                let video = renderer.current_video.as_ref().unwrap();
                assert_eq!((video.width, video.height), (160, 90));
                assert_eq!(video.constants.is_p010, 0);
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { video._texture.GetDesc(&mut desc) };
                assert_eq!(desc.Format, DXGI_FORMAT_R8_UNORM);
                assert_eq!((desc.Width, desc.Height), (160, 90));
                uploads += 1;
            }
        }
        assert_eq!(uploads, 20);
        assert_eq!(renderer.runtime_stats().software_video_frames, uploads);
        assert_eq!(renderer.runtime_stats().hardware_video_frames, 0);
        assert_eq!(renderer.runtime_stats().zero_copy_video_frames, 0);
        renderer.clear_current_frame().unwrap();
        assert!(renderer.current_video.is_none());
    }

    #[test]
    fn flutter_scene_ignores_clock_ticks_but_detects_output_changes() {
        use crate::overlay::OverlayViewport;
        use crate::subtitle::SubtitleBitmapPlane;
        let mut overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(8, 8),
            subtitle_planes: vec![SubtitleBitmapPlane::new(0, 0, 1, 1, vec![255; 4])],
            subtitle_alpha_planes: vec![],
            subtitle_changed: true,
        };
        let scene = FlutterTextureScene::new(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).overlay(Some(&overlay)),
        );
        overlay.pts = Duration::from_secs(2);
        overlay.subtitle_changed = false;
        let context = RenderFrameContext::new(Duration::from_secs(2), 1).overlay(Some(&overlay));
        assert!(scene.matches(7, LumaUpscalerMode::Off, context));
        assert!(!scene.matches(8, LumaUpscalerMode::Off, context));
        assert!(!scene.matches(7, LumaUpscalerMode::ArtCnnC4F16, context));
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2).overlay(Some(&overlay))
        ));
        overlay.subtitle_planes[0].rgba[0] = 0; // Includes HUD pixel changes.
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).overlay(Some(&overlay))
        ));
        assert!(!scene.matches(
            7,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1)
        ));
    }

    #[test]
    fn flutter_scene_reuses_unchanged_cpu_buffers_across_video_frames() {
        use crate::danmaku::DanmakuViewport;
        use crate::overlay::OverlayViewport;
        use crate::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlacement, SubtitleBitmapPlane};
        let mut overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(128, 64),
            subtitle_planes: vec![SubtitleBitmapPlane::new(
                0,
                0,
                128,
                64,
                vec![255; 128 * 64 * 4],
            )],
            subtitle_alpha_planes: vec![SubtitleAlphaBitmap::new(
                SubtitleBitmapPlacement::new(0, 0, 128, 64),
                128,
                0xffffffff,
                vec![255; 128 * 64],
            )],
            subtitle_changed: true,
        };
        let mut plan = DanmakuRenderPlan::empty(Duration::ZERO, 1, DanmakuViewport::new(128, 64));
        plan.items.push(DanmakuGlyphInstance {
            item_id: 1,
            rect: [0.0, 0.0, 1.0, 1.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0; 4],
            outline_rgba: [0.0; 4],
            shadow_rgba: [0.0; 4],
            shadow_offset: [0.0; 2],
        });
        let mut scene = FlutterTextureScene::new(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1)
                .overlay(Some(&overlay))
                .danmaku(Some(&plan)),
        );
        let rgba = scene.overlay.as_ref().unwrap().subtitle_planes[0]
            .rgba
            .as_ptr();
        let alpha = scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0]
            .alpha
            .as_ptr();
        let items = scene.danmaku.as_ref().unwrap().items.as_ptr();
        for token in 2..=121 {
            overlay.pts = Duration::from_millis(token * 10);
            overlay.subtitle_changed = false;
            plan.media_time = overlay.pts;
            let context = RenderFrameContext::new(overlay.pts, 1)
                .overlay(Some(&overlay))
                .danmaku(Some(&plan));
            assert!(!scene.matches(token, LumaUpscalerMode::Off, context));
            scene.update(token, LumaUpscalerMode::Off, context);
            assert!(scene.matches(token, LumaUpscalerMode::Off, context));
            assert_eq!(
                scene.overlay.as_ref().unwrap().subtitle_planes[0]
                    .rgba
                    .as_ptr(),
                rgba
            );
            assert_eq!(
                scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0]
                    .alpha
                    .as_ptr(),
                alpha
            );
            assert_eq!(scene.danmaku.as_ref().unwrap().items.as_ptr(), items);
        }

        // A real subtitle/HUD change still replaces the cached pixels even
        // when the video frame is paused and subtitle_changed is false.
        overlay.subtitle_planes[0].rgba[0] = 17;
        overlay.subtitle_alpha_planes[0].alpha[0] = 23;
        let context = RenderFrameContext::new(overlay.pts, 1)
            .overlay(Some(&overlay))
            .danmaku(Some(&plan));
        assert!(!scene.matches(121, LumaUpscalerMode::Off, context));
        scene.update(121, LumaUpscalerMode::Off, context);
        assert!(scene.matches(121, LumaUpscalerMode::Off, context));
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_planes[0].rgba[0],
            17
        );
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_alpha_planes[0].alpha[0],
            23
        );
        assert_eq!(scene.danmaku.as_ref().unwrap().items.as_ptr(), items);

        let rgba = scene.overlay.as_ref().unwrap().subtitle_planes[0]
            .rgba
            .as_ptr();
        plan.items[0].rect[0] = 2.0;
        let context = RenderFrameContext::new(overlay.pts, 1)
            .overlay(Some(&overlay))
            .danmaku(Some(&plan));
        assert!(!scene.matches(121, LumaUpscalerMode::Off, context));
        scene.update(121, LumaUpscalerMode::Off, context);
        assert!(scene.matches(121, LumaUpscalerMode::Off, context));
        assert_eq!(
            scene.overlay.as_ref().unwrap().subtitle_planes[0]
                .rgba
                .as_ptr(),
            rgba
        );
        assert_eq!(scene.danmaku.as_ref().unwrap().items[0].rect[0], 2.0);

        scene.update(
            121,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2),
        );
        assert!(scene.overlay.is_none() && scene.danmaku.is_none());
        assert!(scene.matches(
            121,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 2)
        ));
    }

    #[test]
    fn flutter_publication_is_sdr_immutable_and_stable_while_idle() {
        use crate::core::FlutterTextureHandle;
        let mut renderer = D3d11Renderer::new().unwrap();
        // WARP keeps this GPU/API contract test runnable on headless CI too.
        let mut device = None;
        let mut device_context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut device_context),
            )
            .unwrap();
        }
        renderer
            .set_device(device.unwrap(), device_context.unwrap())
            .unwrap();
        renderer
            .attach_surface(PlatformSurface::FlutterTexture(FlutterTextureHandle::new(
                FlutterTextureKind::WindowsTextureRegistrar,
                1,
                4,
                4,
                1.0,
            )))
            .unwrap();
        let first = renderer
            .surface
            .as_ref()
            .unwrap()
            .published_texture
            .as_ref()
            .unwrap()
            .clone();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            first.GetDesc(&mut desc);
        }
        assert_eq!(desc.Format, SDR_SWAPCHAIN_FORMAT);
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        assert!(matches!(
            renderer.select_output_mode_for_source(source).unwrap(),
            D3d11OutputMode::Sdr
        ));
        assert_eq!(renderer.stats.hdr10_metadata_failures, 0);
        renderer.render_clear(1.0).unwrap();
        assert_eq!(
            first.as_raw(),
            renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap()
                .as_raw()
        );

        let state = renderer.state.as_ref().unwrap();
        unsafe {
            state.context.ClearRenderTargetView(
                renderer
                    .surface
                    .as_ref()
                    .unwrap()
                    .render_target
                    .as_ref()
                    .unwrap(),
                &[1.0, 0.0, 0.0, 1.0],
            );
        }
        renderer.publish_flutter_texture().unwrap();
        let next = renderer
            .surface
            .as_ref()
            .unwrap()
            .published_texture
            .as_ref()
            .unwrap()
            .clone();
        assert_ne!(first.as_raw(), next.as_raw());
        assert_eq!(
            read_bgra_pixel(renderer.state.as_ref().unwrap(), &first),
            [0, 0, 0, 255]
        );
        assert_eq!(
            read_bgra_pixel(renderer.state.as_ref().unwrap(), &next),
            [0, 0, 255, 255]
        );
        renderer
            .resize_surface(SurfaceMetrics::new(8, 6, 1.0))
            .unwrap();
        unsafe {
            first.GetDesc(&mut desc);
        }
        assert_eq!((desc.Width, desc.Height), (4, 4));
        let mut resized = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            renderer
                .surface
                .as_ref()
                .unwrap()
                .published_texture
                .as_ref()
                .unwrap()
                .GetDesc(&mut resized);
        }
        assert_eq!((resized.Width, resized.Height), (8, 6));
    }

    fn read_bgra_pixel(state: &D3d11DeviceState, texture: &ID3D11Texture2D) -> [u8; 4] {
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            state
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
        }
        let staging = staging.unwrap();
        unsafe {
            state.context.CopyResource(&staging, texture);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            state
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let bytes = *(mapped.pData as *const [u8; 4]);
            state.context.Unmap(&staging, 0);
            bytes
        }
    }

    #[test]
    fn flutter_scene_detects_danmaku_motion_and_visibility_changes() {
        use crate::danmaku::DanmakuViewport;
        let mut plan = DanmakuRenderPlan::empty(Duration::ZERO, 1, DanmakuViewport::new(8, 8));
        plan.items.push(DanmakuGlyphInstance {
            item_id: 1,
            rect: [0.0, 0.0, 1.0, 1.0],
            tex_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgba: [1.0; 4],
            outline_rgba: [0.0; 4],
            shadow_rgba: [0.0; 4],
            shadow_offset: [0.0; 2],
        });
        let scene = FlutterTextureScene::new(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan)),
        );
        plan.media_time = Duration::from_secs(1);
        assert!(scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
        plan.items[0].rect[0] = 2.0;
        assert!(!scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
        plan.items.clear();
        assert!(!scene.matches(
            1,
            LumaUpscalerMode::Off,
            RenderFrameContext::new(Duration::ZERO, 1).danmaku(Some(&plan))
        ));
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected {actual} to be close to {expected}"
        );
    }

    #[test]
    fn video_texture_format_maps_plane_srv_formats() {
        assert_eq!(
            D3d11VideoTextureFormat::from_dxgi(DXGI_FORMAT_NV12)
                .unwrap()
                .luma_srv(),
            DXGI_FORMAT_R8_UNORM
        );
        assert_eq!(
            D3d11VideoTextureFormat::from_dxgi(DXGI_FORMAT_P010)
                .unwrap()
                .chroma_srv(),
            DXGI_FORMAT_R16G16_UNORM
        );
    }

    #[test]
    fn d3d11_tex_rect_crops_padded_decoder_texture() {
        let rect = D3d11TexRect::visible_region(1920, 1080, 2048, 1088);

        assert_close(rect.width, 1920.0 / 2048.0);
        assert_close(rect.height, 1080.0 / 1088.0);
        let vertices = video_vertices(rect);
        assert_close(vertices[0].texcoord[1], 1080.0 / 1088.0);
        assert_close(vertices[2].texcoord[0], 1920.0 / 2048.0);
    }

    #[test]
    fn d3d11_surface_extent_is_already_physical() {
        let metrics = SurfaceMetrics::new(960, 540, 1.5);

        assert_eq!(metrics.physical_size(), (960, 540));
        assert_eq!(metrics.content_scale, 1.5);
    }

    #[test]
    fn d3d11_renderer_config_initializes_upscaler_mode() {
        let renderer = D3d11Renderer::with_config(MetalRendererConfig {
            output_mode: crate::renderer::metal::MetalOutputMode::Sdr,
            luma_upscaler: LumaUpscalerMode::ArtCnnC4F16,
            video_alpha_mode: VideoAlphaMode::Opaque,
        })
        .unwrap();

        let stats = renderer.runtime_stats();
        assert_eq!(stats.upscaler_mode, LumaUpscalerMode::ArtCnnC4F16);
        assert_eq!(stats.upscaler_backend, LumaUpscalerBackendStatus::Building);
    }

    #[test]
    fn d3d11_video_shader_compiles() {
        compile_shader("vs_main", "vs_4_0").unwrap();
        compile_shader("ps_main", "ps_4_0").unwrap();
        compile_shader("encode_ps_main", "ps_4_0").unwrap();
    }

    #[test]
    fn d3d11_overlay_shader_compiles() {
        compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_vs_main", "vs_4_0").unwrap();
        compile_shader_source(OVERLAY_SHADER_SOURCE, "overlay_ps_main", "ps_4_0").unwrap();
    }

    #[test]
    fn renderer_stats_distinguish_direct_and_shared_zero_copy() {
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.stats.hardware_video_frames = 3;
        renderer.stats.zero_copy_video_frames = 3;
        renderer.stats.direct_zero_copy_video_frames = 1;
        renderer.stats.shared_handle_video_frames = 2;
        renderer.stats.hdr_source_frames = 3;
        renderer.stats.hdr10_output_frames = 2;
        renderer.stats.sdr_tonemap_frames = 1;
        renderer.stats.hdr10_output_active = true;

        let stats = renderer.runtime_stats();
        assert_eq!(stats.hardware_video_frames, 3);
        assert_eq!(stats.zero_copy_video_frames, 3);
        assert_eq!(stats.direct_zero_copy_video_frames, 1);
        assert_eq!(stats.shared_handle_video_frames, 2);
        assert_eq!(stats.hdr_source_frames, 3);
        assert_eq!(stats.hdr10_output_frames, 2);
        assert_eq!(stats.sdr_tonemap_frames, 1);
        assert!(stats.hdr10_output_active);
    }

    #[test]
    fn hdr10_output_mode_targets_bt2020_pq() {
        let source = SourceColorState::new(ColorPrimaries::DisplayP3, TransferFunction::Pq);
        let target = D3d11OutputMode::Hdr10.target_color_for_source(source);

        assert_eq!(target.primaries, ColorPrimaries::Bt2020);
        assert_eq!(target.transfer, TransferFunction::Pq);
        assert_eq!(target.peak_nits, 10_000.0);
        assert_eq!(target.reference_white_nits, 203.0);
    }

    #[test]
    fn hdr10_video_constants_request_scene_linear_output() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq);
        let hdr10 = constants_for_frame(
            source,
            D3d11VideoTextureFormat::P010,
            D3d11OutputMode::Hdr10.target_color(),
        );
        let sdr = constants_for_frame(
            source,
            D3d11VideoTextureFormat::P010,
            D3d11OutputMode::Sdr.target_color(),
        );

        assert_eq!(hdr10.scene_linear, 1);
        assert_eq!(sdr.scene_linear, 0);
    }

    #[test]
    fn hlsl_evaluates_reinhard_and_mobius_in_linear_nits() {
        // libplacebo runs these two curves in the linear PL_HDR_NORM domain
        // and only the PQ-domain operators work on encoded values; evaluating
        // them on PQ codes moved the Mobius knee far below its intended
        // position. The HLSL is compiled by the D3D11 runtime, not by the Rust
        // build, so this pins the source shape rather than executing it.
        let source = std::str::from_utf8(SHADER_SOURCE).unwrap();
        for branch in ["if (tone_map == 1u)", "if (tone_map == 2u)"] {
            let start = source.find(branch).expect("branch");
            let rest = &source[start..];
            let end = rest.find("\n    }").expect("branch end");
            let body = &rest[..end];
            assert!(
                body.contains("float out_range_nits = max(dst_peak - dst_black, 0.000001);"),
                "{branch} must scale in linear nits"
            );
            assert!(
                body.contains("nits_from_pq(x)"),
                "{branch} must decode the PQ code before the curve"
            );
            assert!(
                body.contains("return pq_code("),
                "{branch} must re-encode the result"
            );
        }
    }

    #[test]
    fn hlsl_gamut_lut_samples_the_target_black_to_peak_axis() {
        // libplacebo's LUT I axis is [target black, target peak]; a backend
        // that samples `ipt.x / peak` again would diverge from the generated
        // LUT. The lattice position must also be remapped to the texel-center
        // coordinate (libplacebo's `texel_scale`), or the zero-chroma texel at
        // the low end of the C axis crushes saturation.
        let source = std::str::from_utf8(SHADER_SOURCE).unwrap();
        assert!(source.contains("float lut_min = pq_code(tone_map_extra.z);"));
        assert!(source.contains("clamp((ipt.x - lut_min) / lut_range, 0.0, 1.0)"));
        assert!(source.contains("pos.y * (31.0 / 32.0) + 0.5 / 32.0"));
        assert!(source.contains("pos.x * (47.0 / 48.0) + 0.5 / 48.0"));
    }

    #[test]
    fn overlay_uniforms_enable_scene_linear_only_for_hdr10() {
        let hdr10 =
            OverlayUniforms::rgba_plane(0, 0, 1, 1, 1, 1, D3d11OutputMode::Hdr10.target_color());
        let sdr =
            OverlayUniforms::rgba_plane(0, 0, 1, 1, 1, 1, D3d11OutputMode::Sdr.target_color());

        assert_eq!(mem::size_of::<OverlayUniforms>(), 80);
        assert_eq!(hdr10.ui_nits[0], 203.0);
        assert_eq!(hdr10.ui_nits[1], 203.0);
        assert_eq!(sdr.ui_nits[1], 0.0);
    }

    #[test]
    fn dxgi_hdr10_metadata_uses_source_mastering_values() {
        let source = SourceColorState::new(ColorPrimaries::Bt2020, TransferFunction::Pq)
            .hdr_metadata(Some(crate::renderer::pipeline::HdrMetadata::new(
                Some(crate::renderer::pipeline::MasteringDisplayMetadata {
                    display_primaries: Some([
                        Chromaticity::new(0.708, 0.292),
                        Chromaticity::new(0.170, 0.797),
                        Chromaticity::new(0.131, 0.046),
                    ]),
                    white_point: Some(Chromaticity::new(0.3127, 0.3290)),
                    min_luminance_nits: Some(0.005),
                    max_luminance_nits: Some(1000.0),
                }),
                Some(crate::renderer::pipeline::ContentLightMetadata {
                    max_content_light_level_nits: 4000,
                    max_frame_average_light_level_nits: 450,
                }),
            )));

        let metadata = dxgi_hdr10_metadata(source);

        assert_eq!(metadata.RedPrimary, [35400, 14600]);
        assert_eq!(metadata.GreenPrimary, [8500, 39850]);
        assert_eq!(metadata.BluePrimary, [6550, 2300]);
        assert_eq!(metadata.WhitePoint, [15635, 16450]);
        assert_eq!(metadata.MaxMasteringLuminance, 1000);
        assert_eq!(metadata.MinMasteringLuminance, 50);
        assert_eq!(metadata.MaxContentLightLevel, 4000);
        assert_eq!(metadata.MaxFrameAverageLightLevel, 450);
    }

    fn overlay_slot(
        width: u32,
        height: u32,
        bytes: usize,
        last_frame: u64,
    ) -> D3d11OverlayTextureSlot {
        D3d11OverlayTextureSlot {
            width,
            height,
            format: DXGI_FORMAT_R8G8B8A8_UNORM,
            bytes,
            last_frame,
        }
    }

    /// 淘汰策略是纯函数：命中/插入/LRU 淘汰/退回新建四种结果都能在这里钉死，
    /// 不需要 GPU。
    #[test]
    fn overlay_texture_plan_reuses_by_shape_and_protects_the_current_frame() {
        let rgba = DXGI_FORMAT_R8G8B8A8_UNORM;
        let small = overlay_slot(322, 60, 322 * 60 * 4, 7);

        // 上一帧用过的同格式同尺寸条目：命中复用（这就是消掉 6MB/s 抖动的那条路）。
        assert_eq!(
            plan_overlay_texture_slot(&[Some(small)], 322 * 60 * 4, 322, 60, rgba, 322 * 60 * 4, 8),
            D3d11OverlayTexturePlan::Reuse(0)
        );
        // 本帧已交出去的同尺寸条目不能再命中：一帧里两张同尺寸平面共用一张
        // 纹理会让后上传的那张覆盖前一张。
        assert_eq!(
            plan_overlay_texture_slot(
                &[Some(overlay_slot(322, 60, 322 * 60 * 4, 8))],
                322 * 60 * 4,
                322,
                60,
                rgba,
                322 * 60 * 4,
                8
            ),
            D3d11OverlayTexturePlan::Insert(1)
        );
        // 格式不同（R8 的 alpha 图集 vs RGBA 字幕平面）不算命中。
        assert_eq!(
            plan_overlay_texture_slot(
                &[Some(small)],
                322 * 60 * 4,
                322,
                60,
                DXGI_FORMAT_R8_UNORM,
                322 * 60,
                8
            ),
            D3d11OverlayTexturePlan::Insert(1)
        );
        // 尺寸变了但还有空位：插新条目（旧尺寸留着，字幕换行回来还能命中）。
        assert_eq!(
            plan_overlay_texture_slot(&[Some(small)], 322 * 60 * 4, 340, 60, rgba, 340 * 60 * 4, 8),
            D3d11OverlayTexturePlan::Insert(1)
        );
    }

    #[test]
    fn overlay_texture_plan_evicts_lru_but_never_the_frame_in_flight() {
        let rgba = DXGI_FORMAT_R8G8B8A8_UNORM;
        let bytes = 400usize;
        let full: Vec<Option<D3d11OverlayTextureSlot>> = (0..OVERLAY_TEXTURE_CACHE_CAPACITY)
            .map(|index| Some(overlay_slot(10 + index as u32, 10, bytes, 3 + index as u64)))
            .collect();
        let cached = bytes * OVERLAY_TEXTURE_CACHE_CAPACITY;

        // 满了 + 新尺寸：淘汰最久没用过的那条。
        assert_eq!(
            plan_overlay_texture_slot(&full, cached, 99, 99, rgba, bytes, 8),
            D3d11OverlayTexturePlan::Replace(0)
        );
        // 最旧那条属于本帧时改淘汰第二旧的。
        let mut current_first = full.clone();
        current_first[0] = Some(overlay_slot(10, 10, bytes, 8));
        assert_eq!(
            plan_overlay_texture_slot(&current_first, cached, 99, 99, rgba, bytes, 8),
            D3d11OverlayTexturePlan::Replace(1)
        );
        // 整张缓存都属于本帧：谁都不能动，退回按帧新建（与修改前一致）。
        let all_current: Vec<Option<D3d11OverlayTextureSlot>> = (0..OVERLAY_TEXTURE_CACHE_CAPACITY)
            .map(|index| Some(overlay_slot(10 + index as u32, 10, bytes, 8)))
            .collect();
        assert_eq!(
            plan_overlay_texture_slot(&all_current, cached, 99, 99, rgba, bytes, 8),
            D3d11OverlayTexturePlan::Transient
        );
    }

    /// 字节预算：文本字幕平面（0.1MB 级）随便缓存，整屏位图不允许把显存钉死。
    #[test]
    fn overlay_texture_plan_caps_resident_bytes() {
        let rgba = DXGI_FORMAT_R8G8B8A8_UNORM;
        let budget = OVERLAY_TEXTURE_CACHE_BUDGET_BYTES;

        // 单张就超过预算：不进缓存。
        assert_eq!(
            plan_overlay_texture_slot(&[], 0, 3840, 2160, rgba, budget + 1, 1),
            D3d11OverlayTexturePlan::Transient
        );
        // 空缓存里的小平面照常插入。
        assert_eq!(
            plan_overlay_texture_slot(&[], 0, 100, 100, rgba, 64, 1),
            D3d11OverlayTexturePlan::Insert(0)
        );
        // 缓存里已有一条吃满预算的整屏平面：淘汰最旧的小条目也腾不出空间，
        // 宁可让这张退回按帧新建，也不额外钉住第二张整屏平面。
        assert_eq!(
            plan_overlay_texture_slot(
                &[
                    Some(overlay_slot(1, 1, 64, 3)),
                    Some(overlay_slot(2, 2, budget, 4))
                ],
                64 + budget,
                3,
                3,
                rgba,
                budget,
                5
            ),
            D3d11OverlayTexturePlan::Transient
        );
        // 换入更小的平面则允许（释放旧的整屏平面，常驻不增长）。
        assert_eq!(
            plan_overlay_texture_slot(
                &[Some(overlay_slot(2, 2, budget, 4))],
                budget,
                3,
                3,
                rgba,
                64,
                5
            ),
            D3d11OverlayTexturePlan::Replace(0)
        );
    }

    /// 修改前 `append_alpha_atlas_draws` 里 CPU 侧拼接逻辑的逐行副本
    /// （`vec![0u8; atlas_w * atlas_h]` 起手 + 逐行 `copy_from_slice` +
    /// `Vec::new()` placements）。只在测试里当等价性与性能的基线用。
    fn pack_alpha_atlas_baseline(
        bitmaps: &[crate::subtitle::SubtitleAlphaBitmap],
    ) -> Option<(usize, usize, Vec<u8>, Vec<(usize, usize)>)> {
        let mut atlas_width = 0usize;
        let mut atlas_height = 0usize;
        for bitmap in bitmaps {
            if bitmap.placement.width == 0 || bitmap.placement.height == 0 {
                continue;
            }
            atlas_width += bitmap.placement.width as usize;
            atlas_height = atlas_height.max(bitmap.placement.height as usize);
        }
        if atlas_width == 0 || atlas_height == 0 {
            return None;
        }
        let mut pixels = vec![0u8; atlas_width * atlas_height];
        let mut cursor_x = 0usize;
        let mut placements = Vec::new();
        for (index, bitmap) in bitmaps.iter().enumerate() {
            let bw = bitmap.placement.width as usize;
            let bh = bitmap.placement.height as usize;
            if bw == 0 || bh == 0 {
                continue;
            }
            assert!(bitmap.is_valid(), "baseline 只用于合法输入");
            for row in 0..bh {
                let src = row * bitmap.stride;
                let dst = row * atlas_width + cursor_x;
                pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
            }
            placements.push((index, cursor_x));
            cursor_x += bw;
        }
        Some((atlas_width, atlas_height, pixels, placements))
    }

    /// 一张 libass 固定字幕规格的 alpha 位图：宽 828、行 stride 832（行 padding
    /// 4 字节），也就是 `D3d11OverlayTextureEntry::content` 里记的
    /// 「828x57 / 47,420 字节」那张（`832 * 56 + 828 == 47,420`）。
    fn alpha_bitmap_828(
        width: u32,
        height: u32,
        stride: usize,
        seed: u8,
    ) -> crate::subtitle::SubtitleAlphaBitmap {
        use crate::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlacement};
        let len = stride * (height as usize - 1) + width as usize;
        let alpha = (0..len)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect();
        SubtitleAlphaBitmap::new(
            SubtitleBitmapPlacement::new(0, 0, width, height),
            stride,
            0xffff_ffff,
            alpha,
        )
    }

    /// 把整张纹理读回 CPU（staging + `Map`），返回 `(width, height, 逐行紧凑字节)`。
    /// `read_bgra_pixel` 只能读首像素，这里要看整张 alpha 图集。
    fn read_texture_bytes(
        state: &D3d11DeviceState,
        texture: &ID3D11Texture2D,
    ) -> (u32, u32, Vec<u8>) {
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            state
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
        }
        let staging = staging.unwrap();
        unsafe {
            state.context.CopyResource(&staging, texture);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let mut bytes = vec![0u8; desc.Width as usize * desc.Height as usize];
        unsafe {
            state
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            for row in 0..desc.Height as usize {
                let src = mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize);
                let dst = row * desc.Width as usize;
                std::ptr::copy_nonoverlapping(src, bytes[dst..].as_mut_ptr(), desc.Width as usize);
            }
            state.context.Unmap(&staging, 0);
        }
        (desc.Width, desc.Height, bytes)
    }

    /// 一批 `batch` 次操作只取一次 `Instant` 样本，摊掉计时开销
    /// （单次操作在几十纳秒量级时，两次 `Instant::now()` 本身占比可观）。
    struct BenchStats {
        min_ns: f64,
        p50_ns: f64,
        p95_ns: f64,
        mean_ns: f64,
    }

    fn bench_batched(mut body: impl FnMut(), batches: usize, batch: usize) -> BenchStats {
        for _ in 0..batch {
            body();
        }
        let mut samples = Vec::with_capacity(batches);
        for _ in 0..batches {
            let started = Instant::now();
            for _ in 0..batch {
                body();
            }
            samples.push(started.elapsed().as_nanos() as f64 / batch as f64);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).expect("样本不含 NaN"));
        BenchStats {
            min_ns: samples[0],
            p50_ns: samples[samples.len() / 2],
            p95_ns: samples[(samples.len() * 95) / 100],
            mean_ns: samples.iter().sum::<f64>() / samples.len() as f64,
        }
    }

    /// 复用缓冲的等价性契约（纯 CPU，不需要设备）：对同一 `bitmaps`，
    /// 「复用 scratch」产出的 `pixels[..w*h]` 必须与「每次新建 `vec![0u8; w*h]`
    /// 再逐行拷贝」逐字节相同。四个容易出错的输入都覆盖：
    ///   1. 高度不齐 —— 图集里存在任何一次拷贝都写不到的区域（残留像素的唯一入口）；
    ///   2. 尺寸骤变 —— 大 → 小 → 大，缓冲长度比本次用到的前缀长；
    ///   3. 空尺寸平面 —— 跳过但会影响 cursor 累加；
    ///   4. 缓冲被写脏 —— 每轮先把 scratch 填成 0xA5，模拟上一拍留下的像素。
    #[test]
    fn alpha_atlas_scratch_reuses_buffer_with_identical_bytes() {
        use crate::subtitle::SubtitleAlphaBitmap;

        let cases: Vec<Vec<SubtitleAlphaBitmap>> = vec![
            // 真实尺寸：两张 828x57
            vec![
                alpha_bitmap_828(828, 57, 832, 7),
                alpha_bitmap_828(828, 57, 832, 9),
            ],
            // 高度不齐：第二张矮 37 行 → 那 37 行 × 828 字节任何拷贝都写不到
            vec![
                alpha_bitmap_828(828, 57, 832, 3),
                alpha_bitmap_828(828, 20, 832, 5),
            ],
            // 尺寸骤变：大 → 小 → 大
            vec![
                alpha_bitmap_828(828, 57, 832, 11),
                alpha_bitmap_828(828, 57, 832, 13),
            ],
            vec![alpha_bitmap_828(40, 12, 64, 17)],
            vec![
                alpha_bitmap_828(828, 57, 832, 19),
                alpha_bitmap_828(828, 57, 832, 21),
            ],
            // 空尺寸平面（跳过）+ 单像素平面
            vec![alpha_bitmap_828(0, 9, 0, 23), alpha_bitmap_828(1, 1, 1, 29)],
            // 完全没有可拼的平面
            vec![],
        ];

        let mut scratch = D3d11AlphaAtlasScratch::default();
        for (round, case) in cases.iter().enumerate() {
            let baseline = pack_alpha_atlas_baseline(case);
            // 先污染：复用路径必须自己把这些字节清掉。
            scratch.pixels.fill(0xA5);
            scratch.placements.clear();
            scratch.placements.push((usize::MAX, usize::MAX));

            let rebuilt = scratch.rebuild(case).unwrap();
            match baseline {
                None => assert!(!rebuilt, "round {round}: 无可拼平面时应返回 false"),
                Some((width, height, pixels, placements)) => {
                    assert!(rebuilt, "round {round}");
                    assert_eq!(
                        (scratch.width, scratch.height),
                        (width, height),
                        "round {round}: 图集形状"
                    );
                    assert_eq!(scratch.placements, placements, "round {round}: 放置表");
                    assert_eq!(
                        scratch.pixels[..width * height],
                        pixels[..],
                        "round {round}: 复用缓冲的像素必须逐字节等于新建缓冲"
                    );
                }
            }
        }
    }

    /// 端到端（WARP 真设备）：落到**纹理里**的字节也必须与「每拍新建缓冲」的
    /// 基线逐字节相同，覆盖命中缓存的 `UpdateSubresource` 路径与未命中时
    /// `CreateTexture2D(pInitialData)` 路径。第二拍特意让第二张平面变矮：
    /// 图集里那片「任何拷贝都写不到」的区域若残留上一拍的像素，这里会直接读到。
    #[test]
    fn alpha_atlas_texture_bytes_match_new_buffer_baseline() {
        use crate::overlay::OverlayViewport;

        let (device, context) = warp_device();
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.set_device(device, context).unwrap();

        let frames: Vec<Vec<crate::subtitle::SubtitleAlphaBitmap>> = vec![
            // 首拍：新建纹理（pInitialData 路径）
            vec![
                alpha_bitmap_828(828, 57, 832, 7),
                alpha_bitmap_828(828, 57, 832, 9),
            ],
            // 同尺寸不同内容：命中缓存 → UpdateSubresource；第二张矮 → 未覆盖区
            vec![
                alpha_bitmap_828(828, 57, 832, 21),
                alpha_bitmap_828(828, 20, 832, 23),
            ],
            // 换小尺寸：缓存里没有这个形状 → 新建/替换
            vec![alpha_bitmap_828(828, 40, 832, 31)],
            // 回到大尺寸：再次命中缓存 → UpdateSubresource
            vec![
                alpha_bitmap_828(828, 57, 832, 41),
                alpha_bitmap_828(828, 57, 832, 43),
            ],
        ];

        for (frame, bitmaps) in frames.iter().enumerate() {
            let overlay = OverlayFrame {
                pts: Duration::from_millis(frame as u64 * 33),
                viewport: OverlayViewport::new(1920, 1080),
                subtitle_planes: Vec::new(),
                subtitle_alpha_planes: bitmaps.clone(),
                subtitle_changed: true,
            };
            renderer.overlay_textures.begin_frame();
            let draws = renderer.prepare_overlay_draws(Some(&overlay)).unwrap();
            assert_eq!(
                draws.len(),
                bitmaps.len(),
                "frame {frame}: 每张平面一个 draw"
            );
            let atlas_draw = draws.last().unwrap();
            let state = renderer.state.as_ref().unwrap();
            let (width, height, bytes) = read_texture_bytes(state, &atlas_draw.texture._texture);
            let (want_width, want_height, want_pixels, _) =
                pack_alpha_atlas_baseline(bitmaps).unwrap();
            assert_eq!(
                (width as usize, height as usize),
                (want_width, want_height),
                "frame {frame}: 图集纹理尺寸"
            );
            assert_eq!(
                bytes, want_pixels,
                "frame {frame}: 纹理字节必须逐字节等于新建缓冲的基线"
            );
        }
    }

    /// 成本实测：现状（每拍新建缓冲）vs 复用缓冲，并拆出分配 / 清零 / 拷贝各自
    /// 的成本。微秒级量级，**只在 `--release` 下有意义**（debug 会把切片循环与
    /// 边界检查放大数倍）。
    ///
    /// 跑法：
    /// `cargo test --release --lib alpha_atlas_rebuild_cost_is_measured -- --nocapture`
    #[test]
    fn alpha_atlas_rebuild_cost_is_measured() {
        use std::hint::black_box;

        // 真实尺寸：两张 828x57（stride 832）= 47,420 字节/张，
        // 图集 1656x57 = 94,392 字节。
        let bitmaps = vec![
            alpha_bitmap_828(828, 57, 832, 7),
            alpha_bitmap_828(828, 57, 832, 9),
        ];
        let (width, height, baseline_pixels, _) = pack_alpha_atlas_baseline(&bitmaps).unwrap();
        let len = baseline_pixels.len();
        drop(baseline_pixels);

        let mut scratch = D3d11AlphaAtlasScratch::default();
        assert!(scratch.rebuild(&bitmaps).unwrap());
        let mut plain = vec![0u8; len];

        const BATCHES: usize = 200;
        const BATCH: usize = 16;

        let fresh = bench_batched(
            || {
                black_box(pack_alpha_atlas_baseline(black_box(&bitmaps)));
            },
            BATCHES,
            BATCH,
        );
        let reuse = bench_batched(
            || {
                assert!(scratch.rebuild(black_box(&bitmaps)).unwrap());
                black_box(&scratch.pixels[..len]);
            },
            BATCHES,
            BATCH,
        );
        let alloc = bench_batched(
            || {
                let buffer = vec![0u8; black_box(len)];
                black_box(&buffer);
                drop(buffer);
            },
            BATCHES,
            BATCH,
        );
        let memset = bench_batched(
            || {
                scratch.pixels[..len].fill(0);
                black_box(&scratch.pixels[..len]);
            },
            BATCHES,
            BATCH,
        );
        let row_copy = bench_batched(
            || {
                let pixels = &mut plain[..len];
                let mut cursor_x = 0usize;
                for bitmap in black_box(&bitmaps) {
                    let bw = bitmap.placement.width as usize;
                    let bh = bitmap.placement.height as usize;
                    for row in 0..bh {
                        let src = row * bitmap.stride;
                        let dst = row * width + cursor_x;
                        pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
                    }
                    cursor_x += bw;
                }
                black_box(&plain[..len]);
            },
            BATCHES,
            BATCH,
        );
        // 隔离项：同一个函数里「分配 + 清零 + 拷贝」，但不经函数返回、不算放置表。
        // 用来判断现状的耗时里究竟有多少来自「新分配的内存必须被写一遍」这件事本身。
        let alloc_copy = bench_batched(
            || {
                let mut buffer = vec![0u8; black_box(len)];
                let pixels = &mut buffer[..len];
                let mut cursor_x = 0usize;
                for bitmap in black_box(&bitmaps) {
                    let bw = bitmap.placement.width as usize;
                    let bh = bitmap.placement.height as usize;
                    for row in 0..bh {
                        let src = row * bitmap.stride;
                        let dst = row * width + cursor_x;
                        pixels[dst..dst + bw].copy_from_slice(&bitmap.alpha[src..src + bw]);
                    }
                    cursor_x += bw;
                }
                black_box(&buffer[..len]);
            },
            BATCHES,
            BATCH,
        );

        eprintln!(
            "[alpha-atlas bench] 图集 {width}x{height} = {len} 字节（2 张 828x57 / stride 832），\
             每批 {BATCH} 次 × {BATCHES} 批"
        );
        for (label, stats) in [
            ("现状: vec![0;len] + 逐行拷贝 + 放置表", &fresh),
            ("复用: 整块清零 + 逐行拷贝 + 放置表", &reuse),
            ("仅 vec![0;len] 分配+清零+释放", &alloc),
            ("仅 94KB fill(0) 清零（复用缓冲）", &memset),
            ("仅 94KB 逐行拷贝（复用缓冲，57 行×2）", &row_copy),
            ("vec![0;len] + 逐行拷贝（无函数边界/放置表）", &alloc_copy),
        ] {
            eprintln!(
                "[alpha-atlas bench] {label}: min {:.0}ns p50 {:.0}ns p95 {:.0}ns mean {:.0}ns",
                stats.min_ns, stats.p50_ns, stats.p95_ns, stats.mean_ns
            );
        }
        eprintln!(
            "[alpha-atlas bench] 复用相对现状(min)节省 {:.0}ns/拍，相对 p50 节省 {:.0}ns/拍",
            fresh.min_ns - reuse.min_ns,
            fresh.p50_ns - reuse.p50_ns
        );
    }

    /// 别名判断的实测依据：D3D11 不在调用返回后继续引用源缓冲。
    ///
    /// `D3d11AlphaAtlasScratch` 复用同一块缓冲的前提是「两次上传都不会把这块
    /// 内存留作己用」，而这两条上传路径都是 `unsafe` 的裸指针调用、Rust 类型
    /// 系统管不到。这里用真 WARP 设备做行为实验：填好数据 → 调 API →
    /// **立刻把源缓冲改写成另一组字节** → 读回纹理。纹理必须仍是调用时的内容，
    /// 否则复用一块会被下一拍改写的缓冲就是别名风险。
    #[test]
    fn d3d11_overlay_upload_does_not_retain_source_buffer() {
        let (device, context) = warp_device();
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.set_device(device, context).unwrap();

        // 1) CreateTexture2D 的 pInitialData 路径
        let mut initial = vec![0x11u8; 16 * 8];
        let texture = {
            let state = renderer.state.as_ref().unwrap();
            create_cpu_texture(state, 16, 8, DXGI_FORMAT_R8_UNORM, &initial, 16).unwrap()
        };
        initial.fill(0x22);
        {
            let state = renderer.state.as_ref().unwrap();
            let (width, height, bytes) = read_texture_bytes(state, &texture._texture);
            assert_eq!((width, height), (16, 8));
            assert!(
                bytes.iter().all(|byte| *byte == 0x11),
                "CreateTexture2D 返回后改写源缓冲不应影响纹理内容，首 8 字节 {:?}",
                &bytes[..8]
            );
        }

        // 2) UpdateSubresource 的 pSrcData 路径
        let mut update = vec![0x33u8; 16 * 8];
        {
            let state = renderer.state.as_ref().unwrap();
            upload_overlay_texture(state, &texture._texture, &update, 16);
        }
        update.fill(0x44);
        {
            let state = renderer.state.as_ref().unwrap();
            let (_, _, bytes) = read_texture_bytes(state, &texture._texture);
            assert!(
                bytes.iter().all(|byte| *byte == 0x33),
                "UpdateSubresource 返回后改写源缓冲不应影响纹理内容，首 8 字节 {:?}",
                &bytes[..8]
            );
        }
    }

    fn warp_device() -> (ID3D11Device, ID3D11DeviceContext) {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
        }
        (device.unwrap(), context.unwrap())
    }

    /// 真设备契约：同尺寸字幕平面在后续帧复用同一张纹理（不再 `CreateTexture2D`），
    /// 同帧内同尺寸的两张平面各占一张，设备重建后缓存作废。
    #[test]
    fn overlay_texture_cache_reuses_across_frames_and_resets_on_device_swap() {
        use crate::overlay::OverlayViewport;
        use crate::subtitle::{SubtitleAlphaBitmap, SubtitleBitmapPlacement, SubtitleBitmapPlane};
        let rgba_bytes = 4 * 2 * 4;
        let (device, context) = warp_device();
        let device_raw = device.as_raw();
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.set_device(device, context).unwrap();

        let mut overlay = OverlayFrame {
            pts: Duration::ZERO,
            viewport: OverlayViewport::new(16, 16),
            subtitle_planes: vec![
                SubtitleBitmapPlane::new(0, 0, 4, 2, vec![255; rgba_bytes]),
                SubtitleBitmapPlane::new(0, 2, 4, 2, vec![128; rgba_bytes]),
            ],
            subtitle_alpha_planes: vec![SubtitleAlphaBitmap::new(
                SubtitleBitmapPlacement::new(0, 0, 4, 2),
                4,
                0xffffffff,
                vec![255; 4 * 2],
            )],
            subtitle_changed: true,
        };

        renderer.overlay_textures.begin_frame();
        let first = renderer.prepare_overlay_draws(Some(&overlay)).unwrap();
        assert_eq!(first.len(), 3, "2 张 RGBA 平面 + 1 张 alpha 图集");
        assert_eq!(renderer.stats.overlay_plane_texture_creates, 3);
        assert_eq!(renderer.stats.overlay_plane_texture_reuses, 0);
        let first_ids: Vec<*mut c_void> = first
            .iter()
            .map(|draw| draw.texture._texture.as_raw())
            .collect();
        // 旧纹理必须**一直持有到测试结束**：下面要断言"设备重建后拿到的是新对象"，
        // 而裸指针只在对象存活期间才唯一 —— 旧对象一旦被释放，它的地址会被分配器
        // 复用给新对象，于是 `assert_ne!(new.as_raw(), old_raw)` 会随机失败
        // （这条测试此前就是这样偶发失败的，2026-09-15 实测 5 轮里挂 2 轮）。
        // 持有引用之后，两个对象同时存活，指针相等 ⇔ 真的是同一个对象。
        let held_textures: Vec<ID3D11Texture2D> = first
            .iter()
            .map(|draw| draw.texture._texture.clone())
            .collect();
        assert_ne!(
            first_ids[0], first_ids[1],
            "同帧同尺寸的两张字幕平面不能共用一张纹理"
        );
        drop(first);

        // 稳态 120 帧（约 2 秒 60fps）：同尺寸字幕平面全部命中缓存，
        // `CreateTexture2D` 计数恒为 3 —— 这就是"每帧新建纹理、帧末释放"的
        // 6MB/s 抖动被消掉的直接证据。
        for frame in 2..=120u64 {
            overlay.pts = Duration::from_millis(frame * 33);
            renderer.overlay_textures.begin_frame();
            let draws = renderer.prepare_overlay_draws(Some(&overlay)).unwrap();
            let ids: Vec<*mut c_void> = draws
                .iter()
                .map(|draw| draw.texture._texture.as_raw())
                .collect();
            assert_eq!(ids, first_ids, "frame {frame} 应复用同一批纹理");
            assert_eq!(
                renderer.stats.overlay_plane_texture_creates, 3,
                "frame {frame} 不应再新建纹理"
            );
            assert_eq!(
                renderer.stats.overlay_plane_texture_reuses,
                3 * (frame - 1),
                "frame {frame}"
            );
        }
        assert_eq!(
            renderer.overlay_textures.cached_bytes,
            2 * rgba_bytes + 4 * 2
        );

        // 设备重建（TDR 自愈 / 切到解码器设备）：缓存必须清空，新帧必须用新设备
        // 的纹理，而不是旧设备上那张。
        let (next_device, next_context) = warp_device();
        let next_device_raw = next_device.as_raw();
        assert_ne!(device_raw, next_device_raw);
        renderer.set_device(next_device, next_context).unwrap();
        assert!(renderer.overlay_textures.entries.is_empty());

        renderer.overlay_textures.begin_frame();
        let third = renderer.prepare_overlay_draws(Some(&overlay)).unwrap();
        assert_eq!(renderer.stats.overlay_plane_texture_creates, 6);
        let third_texture = &third[0].texture._texture;
        assert!(
            held_textures
                .iter()
                .all(|held| held.as_raw() != third_texture.as_raw()),
            "设备重建后必须新建纹理，不能命中旧设备上那张（旧纹理对象此处仍被持有，\
             地址比较因此是可靠的对象身份比较）"
        );
        assert_ne!(third_texture.as_raw(), first_ids[0]);
        assert_eq!(
            unsafe { third_texture.GetDevice().unwrap() }.as_raw(),
            next_device_raw
        );
    }

    /// 真设备契约（候选 C）：叠加层平面内容与纹理里已有的**逐字节相同**时跳过
    /// `UpdateSubresource`；同一尺寸的内容变了必须真的写进去；**纹理被 LRU 驱逐
    /// 后必须重新上传** —— 静态字幕不能因为"跳过上传"而在驱逐后消失。
    ///
    /// 为什么值得做（实测依据，见 `D3d11OverlayTextureEntry::content`）：一次
    /// `UpdateSubresource` 的成本在下游 flush 边界结算，本机实测 0.20MB 上传使
    /// present p50 +99µs、8.29MB 上传 +822µs；而同一句静态字幕连续 120 拍的内容
    /// 逐字节完全相同（119/119）。逐字节比对 40KB 实测 p50 1.5µs。
    ///
    /// 为什么不能只读计数器：还要读回纹理首像素，证明"该传的时候真的传了、
    /// 该跳的时候一个字节都没被改坏"。
    #[test]
    fn overlay_plane_skips_identical_uploads_and_reeuploads_after_eviction() {
        use crate::overlay::OverlayViewport;
        use crate::subtitle::SubtitleBitmapPlane;

        let (device, context) = warp_device();
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.set_device(device, context).unwrap();

        fn plane(width: u32, height: u32, fill: u8) -> SubtitleBitmapPlane {
            SubtitleBitmapPlane::new(
                0,
                0,
                width,
                height,
                vec![fill; (width * height * 4) as usize],
            )
        }
        fn overlay(planes: Vec<SubtitleBitmapPlane>) -> OverlayFrame {
            OverlayFrame {
                pts: Duration::ZERO,
                viewport: OverlayViewport::new(64, 64),
                subtitle_planes: planes,
                subtitle_alpha_planes: Vec::new(),
                subtitle_changed: true,
            }
        }
        let read = |renderer: &D3d11Renderer, texture: &ID3D11Texture2D| {
            read_bgra_pixel(renderer.state.as_ref().unwrap(), texture)
        };

        // 第 1 帧：走创建路径（`CreateTexture2D` 自带初始数据），不计入上传。
        renderer.overlay_textures.begin_frame();
        let draws = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(4, 2, 9)])))
            .unwrap();
        assert_eq!(renderer.stats.overlay_plane_texture_creates, 1);
        assert_eq!(renderer.stats.overlay_plane_uploads, 0);
        assert_eq!(renderer.stats.overlay_plane_upload_skips, 0);
        let steady = draws[0].texture._texture.clone();
        assert_eq!(read(&renderer, &steady), [9, 9, 9, 9]);

        // 第 2 帧：内容逐字节相同 → 跳过上传（省的是 present 里的提交成本）。
        renderer.overlay_textures.begin_frame();
        let _ = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(4, 2, 9)])))
            .unwrap();
        assert_eq!(renderer.stats.overlay_plane_upload_skips, 1);
        assert_eq!(renderer.stats.overlay_plane_uploads, 0);
        assert_eq!(renderer.stats.overlay_plane_texture_creates, 1);
        assert_eq!(
            read(&renderer, &steady),
            [9, 9, 9, 9],
            "跳过的前提是内容真的没变，像素必须保持原样"
        );

        // 第 3 帧：同一尺寸但内容变了 → 必须真的上传，像素跟着变。
        renderer.overlay_textures.begin_frame();
        let _ = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(4, 2, 7)])))
            .unwrap();
        assert_eq!(renderer.stats.overlay_plane_uploads, 1);
        assert_eq!(renderer.stats.overlay_plane_upload_skips, 1);
        assert_eq!(
            read(&renderer, &steady),
            [7, 7, 7, 7],
            "内容变化必须真的写进纹理，不能沿用旧内容"
        );

        // 塞满缓存（容量 6）→ 再用第 7 种尺寸把最旧的条目（4x2，第 3 帧用过）
        // 挤出去。这一步走的就是 LRU 的 `Replace` 分支。
        for width in [8u32, 12, 16, 20, 24] {
            renderer.overlay_textures.begin_frame();
            let _ = renderer
                .prepare_overlay_draws(Some(&overlay(vec![plane(width, 2, 1)])))
                .unwrap();
        }
        assert_eq!(renderer.overlay_textures.entries.len(), 6);
        renderer.overlay_textures.begin_frame();
        let _ = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(28, 2, 2)])))
            .unwrap();

        // 驱逐之后：同一份静态字幕再回来，**必须重新上传**（新纹理 + 初始数据），
        // 而不是命中"内容已知"的假象。否则用户看到的就是字幕消失。
        let creates_before = renderer.stats.overlay_plane_texture_creates;
        let skips_before = renderer.stats.overlay_plane_upload_skips;
        renderer.overlay_textures.begin_frame();
        let draws = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(4, 2, 9)])))
            .unwrap();
        assert!(
            renderer.stats.overlay_plane_texture_creates > creates_before,
            "被驱逐的尺寸必须重新创建纹理并重传内容"
        );
        assert_eq!(
            renderer.stats.overlay_plane_upload_skips, skips_before,
            "驱逐后不允许再判成「内容未变」"
        );
        let restored = draws[0].texture._texture.clone();
        assert_eq!(
            read(&renderer, &restored),
            [9, 9, 9, 9],
            "驱逐后重新上传必须把静态字幕的像素找回来"
        );

        // 稳态恢复：新条目记下了内容，下一拍同样的内容重新命中跳过。
        renderer.overlay_textures.begin_frame();
        let _ = renderer
            .prepare_overlay_draws(Some(&overlay(vec![plane(4, 2, 9)])))
            .unwrap();
        assert_eq!(renderer.stats.overlay_plane_upload_skips, skips_before + 1);
    }

    /// 真设备契约：视频帧交接栅栏（`wait_for_gpu`）**复用同一个 `ID3D11Query`**，
    /// 不再每个上屏视频帧 `CreateQuery` 一次；设备重建后复用槽必须丢弃重建。
    ///
    /// 锁定方式与 `overlay_texture_cache_reuses_across_frames_and_resets_on_device_swap`
    /// 同构：直接看 `CreateQuery` 计数，而不是"感觉更快了"。真机 30fps 稳态播放每秒
    /// 进这个栅栏 30 次，这里用 120 次等待逼近"2 秒 60fps"。
    ///
    /// 重复 `End` 的合法性依据写在 `wait_for_gpu` 的文档里（`D3D11_QUERY` 枚举与
    /// `GetData` 的原文引用）；本测试补的是**行为证据**：在真实 D3D11 设备（WARP）
    /// 上连续 120 次复用同一个 query，每次 `GetData` 都等到事件完成并返回成功。
    #[test]
    fn gpu_fence_reuses_one_query_across_waits_and_rebuilds_on_device_swap() {
        let (device, context) = warp_device();
        let mut renderer = D3d11Renderer::new().unwrap();
        renderer.set_device(device, context).unwrap();

        // 第 1 次：槽为空，必须新建。
        renderer.wait_for_gpu().unwrap();
        assert_eq!(renderer.stats.gpu_fence_query_creates, 1);
        assert_eq!(renderer.stats.gpu_fence_query_reuses, 0);
        // 旧 query 必须一直被持有：下面要断言"换设备后拿到的是新对象"，而裸指针
        // 只在对象存活期间唯一 —— 旧对象一旦被释放，地址会被分配器复用，断言就
        // 变成随机数（同 `overlay_texture_cache_reuses_...` 里那条注释）。
        let first_held = renderer.gpu_fence_query.as_ref().unwrap().clone();
        let first = first_held.as_raw();

        // 稳态 120 次：同一个 query 反复 End/GetData。
        for call in 2..=120u64 {
            renderer.wait_for_gpu().unwrap();
            assert_eq!(
                renderer.stats.gpu_fence_query_creates, 1,
                "call {call}: 复用生效时不应再 CreateQuery"
            );
            assert_eq!(
                renderer.stats.gpu_fence_query_reuses,
                call - 1,
                "call {call}"
            );
            assert_eq!(
                renderer.gpu_fence_query.as_ref().unwrap().as_raw(),
                first,
                "call {call}: 复用的必须是同一个 query 对象"
            );
        }
        assert_eq!(renderer.stats.gpu_fence_query_reuses, 119);

        // 设备重建（TDR 自愈 / 切到解码器设备）：query 属于创建它的设备，必须丢弃。
        let (next_device, next_context) = warp_device();
        renderer.set_device(next_device, next_context).unwrap();
        assert!(renderer.gpu_fence_query.is_none(), "换设备后复用槽必须清空");
        renderer.wait_for_gpu().unwrap();
        assert_eq!(renderer.stats.gpu_fence_query_creates, 2);
        assert_eq!(renderer.stats.gpu_fence_query_reuses, 119);
        assert_ne!(
            renderer.gpu_fence_query.as_ref().unwrap().as_raw(),
            first_held.as_raw(),
            "新设备上的栅栏 query 必须是新对象（旧对象此处仍被持有，指针比较可靠）"
        );
    }
}
